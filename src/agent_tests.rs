#[cfg(test)]
mod tests {
    use super::extensions_integration_tests::PiAiCaptureProvider;
    use super::*;
    use crate::auth::AuthCredential;
    use crate::provider::{InputType, Model, ModelCost};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc as StdArc, Mutex as StdTestMutex};

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn queued_user_message(text: &str) -> QueuedAgentMessage {
        QueuedAgentMessage::from_authored_message(user_message(text))
    }

    fn assert_user_text(message: &Message, expected: &str) {
        assert!(
            matches!(
                message,
                Message::User(UserMessage {
                    content: UserContent::Text(_),
                    ..
                })
            ),
            "expected user text message, got {message:?}"
        );
        if let Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) = message
        {
            assert_eq!(text, expected);
        }
    }

    fn sample_image_block() -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        })
    }

    fn image_count_in_message(message: &Message) -> usize {
        let count_images = |blocks: &[ContentBlock]| {
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count()
        };
        match message {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => count_images(blocks),
            Message::Assistant(msg) => count_images(&msg.content),
            Message::ToolResult(tool_result) => count_images(&tool_result.content),
            Message::User(UserMessage {
                content: UserContent::Text(_),
                ..
            })
            | Message::Custom(_) => 0,
        }
    }

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SilentProvider {
        fn name(&self) -> &str {
            "silent-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct CountingStopProvider {
        stream_calls: StdArc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CountingStopProvider {
        fn name(&self) -> &str {
            "counting-stop-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_message("done"),
            })])))
        }
    }

    /// Reports token usage (and optionally a billed total) plus catalog
    /// pricing, so a finished turn must carry a priced `usage.cost` (gh #221).
    #[derive(Debug)]
    struct PricedUsageProvider {
        provider_reported_total: Option<f64>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for PricedUsageProvider {
        fn name(&self) -> &str {
            "priced-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        fn model_cost(&self) -> Option<crate::provider::ModelCost> {
            Some(crate::provider::ModelCost {
                input: 2.0,
                output: 10.0,
                cache_read: 0.2,
                cache_write: 2.5,
            })
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let mut message = assistant_message("done");
            message.usage = Usage {
                input: 1_000_000,
                output: 100_000,
                cache_read: 500_000,
                cache_write: 0,
                total_tokens: 1_600_000,
                cost: crate::model::Cost {
                    total: self.provider_reported_total.unwrap_or(0.0),
                    ..crate::model::Cost::default()
                },
            };
            Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            })])))
        }
    }

    fn run_priced_turn(provider_reported_total: Option<f64>) -> crate::model::Usage {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = StdArc::new(PricedUsageProvider {
                provider_reported_total,
            });
            let mut agent = Agent::new(
                provider,
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let ended = StdArc::new(std::sync::Mutex::new(None));
            let ended_for_events = StdArc::clone(&ended);
            agent
                .run("price me", move |event| {
                    if let AgentEvent::MessageEnd {
                        message: Message::Assistant(message),
                    } = event
                    {
                        *ended_for_events.lock().unwrap() = Some(message.usage.clone());
                    }
                })
                .await
                .expect("agent run");
            let usage = ended.lock().unwrap().clone().expect("message_end usage");
            let stored = match agent.messages().last() {
                Some(Message::Assistant(message)) => message.usage.clone(),
                other => panic!("expected assistant message, got {other:?}"),
            };
            assert!(
                (stored.cost.total - usage.cost.total).abs() < 1e-12,
                "history and message_end must agree"
            );
            usage
        })
    }

    /// gh #221: `usage.cost` is priced from catalog rates on every finished
    /// assistant message; previously it stayed 0.0 for every provider.
    #[test]
    fn finished_turn_prices_usage_from_catalog_rates() {
        let usage = run_priced_turn(None);
        assert!((usage.cost.input - 2.0).abs() < 1e-9);
        assert!((usage.cost.output - 1.0).abs() < 1e-9);
        assert!((usage.cost.cache_read - 0.1).abs() < 1e-9);
        assert!(usage.cost.cache_write.abs() < 1e-12);
        assert!((usage.cost.total - 3.1).abs() < 1e-9);
    }

    /// gh #221: a billed total reported by the provider (OpenRouter
    /// `usage.cost`) survives pricing as the authoritative total.
    #[test]
    fn finished_turn_keeps_provider_reported_cost_total() {
        let usage = run_priced_turn(Some(2.75));
        assert!((usage.cost.total - 2.75).abs() < 1e-12);
        assert!((usage.cost.input - 2.0).abs() < 1e-9);
    }

    #[test]
    fn background_job_follow_ups_track_the_live_agent_session() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let first_session = Session::in_memory();
            let first_id = first_session.header.id.clone();
            let second_id = format!("session-b-{}", uuid::Uuid::new_v4().simple());
            let session = Arc::new(Mutex::new(first_session));
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            crate::jobs::push_completion_notice(&first_id, "first-session-notice")
                .expect("first notice");
            crate::jobs::push_completion_notice(&second_id, "second-session-notice")
                .expect("second notice");
            agent_session
                .agent
                .fetch_additive_follow_up_messages()
                .await;
            assert!(
                agent_session.agent.has_staged_follow_up(),
                "the first owner's notice must be staged before the session switch"
            );

            {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                guard.header.id.clone_from(&second_id);
            }
            let stale_delivery = agent_session
                .agent
                .pop_follow_up_for_current_session()
                .await;
            assert!(
                stale_delivery.is_empty(),
                "delivery-time owner validation must not expose the staged first-session notice"
            );
            agent_session
                .agent
                .fetch_additive_follow_up_messages()
                .await;
            let second_delivery = agent_session
                .agent
                .pop_follow_up_for_current_session()
                .await;
            assert_eq!(second_delivery.len(), 1);
            assert_user_text(second_delivery[0].message(), "second-session-notice");
            assert!(second_delivery.iter().all(|delivery| {
                !matches!(
                    delivery.message(),
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == "first-session-notice"
                )
            }));

            {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                guard.header.id.clone_from(&first_id);
            }
            agent_session
                .agent
                .fetch_additive_follow_up_messages()
                .await;
            let restored_first_delivery = agent_session
                .agent
                .pop_follow_up_for_current_session()
                .await;
            assert_eq!(restored_first_delivery.len(), 1);
            assert_user_text(restored_first_delivery[0].message(), "first-session-notice");
            assert!(crate::jobs::take_completion_notices(&first_id).is_empty());
            assert!(crate::jobs::take_completion_notices(&second_id).is_empty());
        });
    }

    #[test]
    fn stale_only_job_handoff_does_not_invoke_the_provider_empty() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let first_id = format!("stale-owner-a-{}", uuid::Uuid::new_v4().simple());
            let second_id = format!("stale-owner-b-{}", uuid::Uuid::new_v4().simple());
            let resolver_calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
            let resolver_calls_for_scope = StdArc::clone(&resolver_calls);
            let first_id_for_scope = first_id.clone();
            let second_id_for_scope = second_id.clone();
            let tools = ToolRegistry::from_tools(Vec::new());
            tools.bind_job_session_resolver(StdArc::new(move || {
                let resolved = if resolver_calls_for_scope.fetch_add(1, Ordering::SeqCst) == 0 {
                    first_id_for_scope.clone()
                } else {
                    second_id_for_scope.clone()
                };
                Box::pin(async move { Some(resolved) })
            }));
            let stream_calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
            let provider = StdArc::new(CountingStopProvider {
                stream_calls: StdArc::clone(&stream_calls),
            });
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            let turn_starts = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
            let turn_starts_for_events = StdArc::clone(&turn_starts);

            crate::jobs::push_completion_notice(&first_id, "stale-only-notice")
                .expect("first owner notice");
            agent
                .run("initial prompt", move |event| {
                    if matches!(event, AgentEvent::TurnStart { .. }) {
                        turn_starts_for_events.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .await
                .expect("agent run");

            assert_eq!(
                stream_calls.load(Ordering::SeqCst),
                1,
                "a stale-only staged batch must not create an empty provider turn"
            );
            assert_eq!(
                turn_starts.load(Ordering::SeqCst),
                1,
                "a stale-only staged batch must not emit an unmatched turn start"
            );
            let restored = crate::jobs::take_completion_notices(&first_id);
            assert_eq!(restored.len(), 1);
            assert_user_text(&restored[0], "stale-only-notice");
            assert!(crate::jobs::take_completion_notices(&second_id).is_empty());
        });
    }

    #[test]
    fn background_job_notice_survives_a_full_additive_handoff() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session_state = Session::in_memory();
            let session_id = session_state.header.id.clone();
            let session = Arc::new(Mutex::new(session_state));
            let mut agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let ordinary_fetcher: MessageFetcher = Arc::new(|| {
                Box::pin(async move {
                    (0..=MAX_FOLLOW_UP_QUEUE_SIZE)
                        .map(|index| {
                            QueuedAgentMessage::generated(Message::User(UserMessage {
                                content: UserContent::Text(format!("ordinary-{index}")),
                                timestamp: 0,
                            }))
                        })
                        .collect()
                })
            });
            agent.register_message_fetchers(None, Some(ordinary_fetcher));
            agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::All);
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            crate::jobs::push_completion_notice(&session_id, "bounded-job-notice")
                .expect("job notice");
            agent_session
                .agent
                .fetch_additive_follow_up_messages()
                .await;
            let deliveries = agent_session.agent.message_queue.pop_follow_up();
            assert_eq!(deliveries.len(), MAX_FOLLOW_UP_QUEUE_SIZE + 1);
            assert!(deliveries.iter().any(|delivery| matches!(
                delivery.message(),
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) if text == "bounded-job-notice"
            )));
        });
    }

    #[test]
    fn busy_owning_follow_up_source_does_not_starve_job_notices() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session_state = Session::in_memory();
            let session_id = session_state.header.id.clone();
            let session = Arc::new(Mutex::new(session_state));
            let mut agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let owning_fetcher: MessageFetcher = Arc::new(|| {
                Box::pin(async move {
                    vec![QueuedAgentMessage::generated(Message::User(UserMessage {
                        content: UserContent::Text("owning-follow-up".to_string()),
                        timestamp: 0,
                    }))]
                })
            });
            agent.register_initial_follow_up_fetcher(owning_fetcher);
            agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::All);
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            crate::jobs::push_completion_notice(&session_id, "non-starved-job-notice")
                .expect("job notice");
            assert!(agent_session.agent.stage_follow_up_messages().await);
            let deliveries = agent_session.agent.message_queue.pop_follow_up();
            assert_eq!(deliveries.len(), 2);
            assert_user_text(deliveries[0].message(), "owning-follow-up");
            assert_user_text(deliveries[1].message(), "non-starved-job-notice");
            assert!(crate::jobs::take_completion_notices(&session_id).is_empty());
        });
    }

    #[test]
    fn one_at_a_time_job_handoff_keeps_only_one_registry_batch_staged() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session_state = Session::in_memory();
            let session_id = session_state.header.id.clone();
            let session = Arc::new(Mutex::new(session_state));
            let mut agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::OneAtATime);
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            for round in 0..3 {
                for index in 0..crate::jobs::MAX_COMPLETION_NOTICES_PER_SESSION {
                    crate::jobs::push_completion_notice(
                        &session_id,
                        format!("job-notice-{round}-{index}"),
                    )
                    .expect("job notice");
                }

                assert!(agent_session.agent.stage_follow_up_messages().await);
                let staged_jobs = agent_session
                    .agent
                    .message_queue
                    .follow_up
                    .iter()
                    .filter(|entry| entry.job_owner_session_id.is_some())
                    .count();
                assert!(
                    staged_jobs <= crate::jobs::MAX_COMPLETION_NOTICES_PER_SESSION,
                    "the Agent queue must retain at most one bounded registry batch"
                );
                assert_eq!(
                    agent_session.agent.message_queue.pop_follow_up().len(),
                    1,
                    "one-at-a-time mode must consume exactly one staged notice"
                );
            }

            let retained_registry_batch = crate::jobs::take_completion_notices(&session_id);
            assert_eq!(
                retained_registry_batch.len(),
                crate::jobs::MAX_COMPLETION_NOTICES_PER_SESSION,
                "the registry must retain only its independent bounded batch"
            );
        });
    }

    #[derive(Debug)]
    struct DeltaOnlyProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for DeltaOnlyProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[derive(Debug, Clone, Default)]
    struct CapturedProviderContext {
        system_prompt: Option<String>,
        messages: Vec<Message>,
        thinking_level: Option<crate::model::ThinkingLevel>,
    }

    #[derive(Debug)]
    struct CapturingProvider {
        api: &'static str,
        calls: StdArc<StdTestMutex<Vec<CapturedProviderContext>>>,
    }

    impl CapturingProvider {
        fn new(api: &'static str) -> Self {
            Self {
                api,
                calls: StdArc::new(StdTestMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> StdArc<StdTestMutex<Vec<CapturedProviderContext>>> {
            StdArc::clone(&self.calls)
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing-provider"
        }

        fn api(&self) -> &str {
            self.api
        }

        fn model_id(&self) -> &str {
            "capture-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls
                .lock()
                .expect("capture context lock")
                .push(CapturedProviderContext {
                    system_prompt: context.system_prompt.as_ref().map(ToString::to_string),
                    messages: context.messages.iter().cloned().collect(),
                    thinking_level: options.thinking_level,
                });
            let final_message = assistant_message("captured");
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                },
            )])))
        }
    }

    fn sample_semantic_context_bundle() -> SemanticContextBundle {
        use crate::semantic_workspace_graph::{
            ContextBundleBudget, ContextBundleExclusion, ContextBundleInvalidationPolicy,
            ContextRedactionSummary, EvidenceFreshnessStatus, RedactionStatus, SemanticNodeType,
        };

        SemanticContextBundle {
            schema: crate::semantic_workspace_graph::SEMANTIC_CONTEXT_BUNDLE_SCHEMA.to_string(),
            budget: ContextBundleBudget {
                max_items: 8,
                max_bytes: 4096,
            },
            selected_items: vec![
                ContextBundleItem {
                    node_id: "node-session".to_string(),
                    node_type: SemanticNodeType::CodeSymbol,
                    source_path: "src/agent.rs".to_string(),
                    title: "AgentSession::run_agent_with_text".to_string(),
                    reason: "query_match,related_to_bead_or_changed_path".to_string(),
                    score: 420,
                    estimated_bytes: 700,
                    estimated_tokens: 175,
                    freshness_status: None,
                    redaction_status: RedactionStatus::None,
                },
                ContextBundleItem {
                    node_id: "node-test".to_string(),
                    node_type: SemanticNodeType::TestCase,
                    source_path: "tests/agent_loop_reliability.rs".to_string(),
                    title: "semantic context session coverage".to_string(),
                    reason: "validation_context".to_string(),
                    score: 300,
                    estimated_bytes: 400,
                    estimated_tokens: 100,
                    freshness_status: Some(EvidenceFreshnessStatus::Current),
                    redaction_status: RedactionStatus::Redacted,
                },
            ],
            excluded_items: vec![ContextBundleExclusion {
                node_id: "stale-doc".to_string(),
                node_type: SemanticNodeType::DocSection,
                source_path: "README.md".to_string(),
                title: "obsolete drop-in claim".to_string(),
                reason: "suppressed_stale_or_unsafe_evidence".to_string(),
                score: 250,
                estimated_bytes: 300,
                freshness_status: Some(EvidenceFreshnessStatus::Uncertified),
                redaction_status: RedactionStatus::SensitiveOmitted,
            }],
            stale_evidence_suppressions: Vec::new(),
            redaction_summary: ContextRedactionSummary {
                policy_version: "test-policy".to_string(),
                overall_status: RedactionStatus::Redacted,
                selected_redacted_nodes: 1,
                selected_sensitive_omissions: 0,
                suppressed_unsafe_nodes: 0,
                redacted_metadata_keys: BTreeSet::from(["api_key".to_string()]),
                sensitive_path_kinds: BTreeSet::new(),
            },
            invalidation_policy: ContextBundleInvalidationPolicy {
                policy_version: "test-policy".to_string(),
                workspace_id: "workspace:test".to_string(),
                branch: Some("main".to_string()),
                session_id: Some("session-123".to_string()),
                input_fingerprint_sha256: "abc123".repeat(10),
                cache_ttl_seconds: 900,
                generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
                expires_at_utc: Some("2026-05-13T00:15:00Z".to_string()),
                invalidates_on: vec!["input_fingerprint_change".to_string()],
                cacheable: true,
            },
            path_normalization: Vec::new(),
            suggested_validation_commands: vec![
                "cargo test agent_semantic_context".to_string(),
                "cargo check --all-targets".to_string(),
            ],
            estimated_bytes: 1100,
            estimated_tokens: 275,
        }
    }

    #[test]
    fn semantic_context_exclusion_prompt_deduplicates_before_row_cap() {
        let mut bundle = sample_semantic_context_bundle();
        let duplicate = bundle.excluded_items[0].clone();
        bundle.stale_evidence_suppressions.push(duplicate);

        for index in 0..8 {
            let mut exclusion = bundle.excluded_items[0].clone();
            exclusion.node_id = format!("excluded-{index}");
            exclusion.source_path = format!("docs/evidence/excluded-{index}.json");
            exclusion.title = format!("excluded evidence {index}");
            bundle.excluded_items.push(exclusion);
        }

        let mut prompt = String::new();
        let mut stats = SemanticContextPromptStats::default();
        push_semantic_context_exclusions(
            &mut prompt,
            &mut stats,
            SemanticContextPromptBudget {
                max_items: 16,
                max_bytes: 64 * 1024,
            },
            &bundle,
        );

        assert_eq!(
            prompt
                .matches(
                    "README.md :: obsolete drop-in claim reason=suppressed_stale_or_unsafe_evidence",
                )
                .count(),
            1,
            "the same exclusion from both bundle collections must render once"
        );
        assert_eq!(stats.exclusions_included, 8);
        assert_eq!(
            stats.exclusions_omitted, 1,
            "the row cap must count omitted unique exclusions, not raw duplicate records"
        );
    }

    #[test]
    fn delta_without_start_does_not_mutate_previous_message() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(DeltaOnlyProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(provider, tools, AgentConfig::default());

            agent.add_message(Message::Assistant(Arc::new(assistant_message("prev"))));

            agent
                .run_with_message_with_abort(user_message("hi"), None, |_| {})
                .await
                .expect("run");

            let assistant_texts = agent
                .messages()
                .iter()
                .filter_map(|message| match message {
                    Message::Assistant(msg)
                        if matches!(msg.content.as_slice(), [ContentBlock::Text(_)]) =>
                    {
                        if let [ContentBlock::Text(text)] = msg.content.as_slice() {
                            Some(text.text.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(
                assistant_texts.as_slice(),
                ["prev".to_string(), "hello".to_string()]
            );
        });
    }

    /// Feed terminal protocol cases through the real Agent loop, then stop so
    /// a pre-check regression fails on observable tool effects instead of looping.
    struct TerminalBatchProvider {
        events: Vec<StreamEvent>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TerminalBatchProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let events = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.events.clone()
            } else {
                vec![StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_message("finished"),
                }]
            };
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
        }
    }

    fn batch_write_call(id: &str, path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "write".to_string(),
            arguments: json!({"path": path, "content": content}),
            thought_signature: Some("provider-call-signature".to_string()),
        }
    }

    struct RuleScriptProvider {
        scripts: Vec<Vec<StreamEvent>>,
        calls: std::sync::atomic::AtomicUsize,
        requests: StdMutex<Vec<Vec<Message>>>,
        active_streams: Arc<std::sync::atomic::AtomicUsize>,
        polls: Vec<Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl RuleScriptProvider {
        fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
            let polls = scripts
                .iter()
                .map(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
                .collect();
            Self {
                scripts,
                calls: std::sync::atomic::AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                polls,
            }
        }
    }

    struct RuleScriptStream {
        events: VecDeque<StreamEvent>,
        active_streams: Arc<std::sync::atomic::AtomicUsize>,
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Stream for RuleScriptStream {
        type Item = Result<StreamEvent>;

        fn poll_next(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = self.get_mut();
            this.polls.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(this.events.pop_front().map(Ok))
        }
    }

    impl Drop for RuleScriptStream {
        fn drop(&mut self) {
            self.active_streams.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for RuleScriptProvider {
        fn name(&self) -> &str {
            "rule-fixture"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            self.requests
                .lock()
                .expect("request capture")
                .push(context.messages.to_vec());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let script = self
                .scripts
                .get(call)
                .ok_or_else(|| Error::api("unexpected stream-rule provider request"))?;
            self.active_streams
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .map_err(|_| Error::api("previous provider stream was not dropped"))?;
            Ok(Box::pin(RuleScriptStream {
                events: script.clone().into(),
                active_streams: Arc::clone(&self.active_streams),
                polls: Arc::clone(&self.polls[call]),
            }))
        }
    }

    fn rule_fixture() -> crate::stream_rules::StreamRule {
        crate::stream_rules::StreamRule {
            id: "native-api".to_string(),
            name: "Native API".to_string(),
            pattern: "FORBIDDEN".to_string(),
            body: "Use the supported API instead.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        }
    }

    fn rule_done(text: &str) -> Vec<StreamEvent> {
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant_message(text),
        }]
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn stream_rules_discard_tool_drafts_before_execution_and_persist_the_directive() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace");
            let call = batch_write_call("discarded", "target.txt", "must not run");
            let mut terminal = assistant_message("FORBIDDEN");
            terminal.content.push(ContentBlock::ToolCall(call.clone()));
            let provider = Arc::new(RuleScriptProvider::new(vec![
                vec![
                    StreamEvent::Start {
                        partial: assistant_message(""),
                    },
                    StreamEvent::ToolCallEnd {
                        content_index: 1,
                        tool_call: call,
                    },
                    StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "FORB".to_string(),
                    },
                    StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "IDDEN".to_string(),
                    },
                    StreamEvent::Done {
                        reason: StopReason::ToolUse,
                        message: terminal,
                    },
                ],
                rule_done("Completed the supported approach."),
            ]));
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&["write"], temp.path(), None),
                AgentConfig::default(),
            );
            agent.set_stream_rules(vec![rule_fixture()]).expect("rules");
            let saved = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("sessions"),
            ))));
            let mut session = AgentSession::new(
                agent,
                Arc::clone(&saved),
                true,
                ResolvedCompactionSettings {
                    enabled: false,
                    ..ResolvedCompactionSettings::default()
                },
            );
            let events = Arc::new(StdMutex::new(Vec::new()));
            let capture = Arc::clone(&events);
            let result = session
                .run_text_with_abort("Make the change".to_string(), None, move |event| {
                    capture.lock().expect("events").push(event);
                })
                .await
                .expect("corrected turn");
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert!(!temp.path().join("target.txt").exists());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
            assert_eq!(provider.polls[0].load(Ordering::SeqCst), 4);
            assert_eq!(provider.active_streams.load(Ordering::SeqCst), 0);
            {
                let requests = provider.requests.lock().expect("requests");
                let retry = serde_json::to_string(&requests[1]).expect("serialize request");
                assert!(retry.contains("Use the supported API instead."));
                assert!(!retry.contains("FORB"));
                assert!(!retry.contains("discarded\""));
                assert!(requests[1].iter().all(|message| !matches!(
                    message,
                    Message::Assistant(_) | Message::ToolResult(_)
                )));
                drop(requests);
            }
            {
                let events = events.lock().expect("events");
                assert!(!events.iter().any(|event| matches!(
                    event,
                    AgentEvent::ToolExecutionStart { .. } | AgentEvent::ProviderError { .. }
                )));
                assert_eq!(
                    events.iter().filter(|event| matches!(event, AgentEvent::AgentEnd { .. })).count(),
                    1
                );
                let starts = events.iter().filter(|event| matches!(event,
                    AgentEvent::MessageStart { message: Message::Assistant(_) }
                )).count();
                let ends = events.iter().filter(|event| matches!(event,
                    AgentEvent::MessageEnd { message: Message::Assistant(_) }
                )).count();
                assert_eq!((starts, ends), (2, 2));
                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::AgentEnd { messages, error: None, .. }
                        if messages.iter().any(|message| matches!(message,
                            Message::Custom(custom) if custom.custom_type == crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE
                        ))
                )));
                drop(events);
            }
            let path = {
                let cx = crate::agent_cx::AgentCx::for_request();
                saved.lock(cx.cx()).await.expect("saved session").path.clone().expect("path")
            };
            let reopened = Session::open(path.to_string_lossy().as_ref()).await.expect("reopen");
            let history = reopened.to_messages_for_current_path();
            assert_eq!(history.len(), 3);
            let Message::Custom(reminder) = &history[1] else {
                panic!("durable stream-rule directive missing");
            };
            assert_eq!(reminder.custom_type, crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE);
            assert_eq!(reminder.details.as_ref().expect("provenance")["ruleId"], "native-api");
            assert!(!serde_json::to_string(&history).expect("history").contains("FORB"));
        });
    }

    #[test]
    fn stream_rule_cap_survives_continuation_but_a_new_prompt_gets_an_allowance() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let mut scripts = vec![rule_done("FORBIDDEN"); 6];
            scripts.push(rule_done("Completed."));
            let provider = Arc::new(RuleScriptProvider::new(scripts));
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_stream_rules(vec![rule_fixture()]).expect("rules");
            let stopped = agent.run("first prompt", |_| {}).await.expect("bounded turn");
            assert_eq!(stopped.stop_reason, StopReason::Aborted);
            assert!(stopped.error_message.as_deref().expect("cap diagnostic").contains("PI_STREAM_RULE_LIMIT"));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
            let continued = agent.run_continue_with_abort(None, |_| {}).await.expect("continuation");
            assert_eq!(continued.stop_reason, StopReason::Aborted);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
            let next = agent.run("new authored prompt", |_| {}).await.expect("fresh turn");
            assert_eq!(next.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 7);
            assert_eq!(agent.messages().iter().filter(|message| matches!(message,
                Message::Custom(custom) if custom.custom_type == crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE
            )).count(), 4);
        });
    }

    #[test]
    fn abort_after_a_stream_rule_reminder_does_not_open_a_retry_stream() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let provider = Arc::new(RuleScriptProvider::new(vec![rule_done("FORBIDDEN")]));
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_stream_rules(vec![rule_fixture()]).expect("rules");
            let (abort, signal) = AbortHandle::new();
            let result = agent.run_with_abort("prompt", Some(signal), move |event| {
                if matches!(event, AgentEvent::MessageEnd { message: Message::Custom(_) }) {
                    abort.abort();
                }
            }).await.expect("cooperative abort");
            assert_eq!(result.stop_reason, StopReason::Aborted);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(provider.active_streams.load(Ordering::SeqCst), 0);
            assert!(agent.messages().iter().any(|message| matches!(message,
                Message::Custom(custom) if custom.custom_type == crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE
            )));
        });
    }

    #[test]
    fn stream_rules_inspect_terminal_and_thinking_text_without_inspecting_tool_arguments() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for thinking in [false, true] {
                let mut message = assistant_message("FORBIDDEN");
                if thinking {
                    message.content = vec![ContentBlock::Thinking(ThinkingContent {
                        thinking: "FORBIDDEN".to_string(),
                        thinking_signature: None,
                    })];
                }
                let provider = Arc::new(RuleScriptProvider::new(vec![
                    vec![StreamEvent::Done { reason: StopReason::Stop, message }],
                    rule_done("Completed."),
                ]));
                let mut agent = Agent::new(provider.clone(), ToolRegistry::from_tools(Vec::new()), AgentConfig::default());
                agent.set_stream_rules(vec![rule_fixture()]).expect("rules");
                assert_eq!(agent.run("prompt", |_| {}).await.expect("turn").stop_reason, StopReason::Stop);
                assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
            }

            let temp = tempfile::tempdir().expect("workspace");
            let mut message = assistant_message("Write the requested text.");
            message.content.push(ContentBlock::ToolCall(batch_write_call("allowed", "target.txt", "FORBIDDEN")));
            let provider = Arc::new(RuleScriptProvider::new(vec![
                vec![StreamEvent::Done { reason: StopReason::ToolUse, message }],
                rule_done("Completed."),
            ]));
            let mut agent = Agent::new(provider.clone(), ToolRegistry::new(&["write"], temp.path(), None), AgentConfig::default());
            agent.set_stream_rules(vec![rule_fixture()]).expect("rules");
            agent.run("write the literal", |_| {}).await.expect("tool turn");
            assert_eq!(std::fs::read_to_string(temp.path().join("target.txt")).expect("tool output"), "FORBIDDEN");
            assert!(!agent.messages().iter().any(|message| matches!(message,
                Message::Custom(custom) if custom.custom_type == crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE
            )));
        });
    }

    #[test]
    fn stream_rule_source_reloads_between_prompts_and_refuses_invalid_replacements() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace");
            let global = temp.path().join("global");
            std::fs::create_dir_all(&global).expect("global directory");
            let provider = Arc::new(RuleScriptProvider::new(vec![
                rule_done("FORBIDDEN"), rule_done("FORBIDDEN"), rule_done("Completed."),
            ]));
            let mut agent = Agent::new(provider.clone(), ToolRegistry::from_tools(Vec::new()), AgentConfig::default());
            agent.set_stream_rule_source(crate::stream_rules::StreamRuleSource::new(
                temp.path().to_path_buf(), global.clone(), false,
            )).expect("source");
            agent.run("before configuration", |_| {}).await.expect("unconfigured turn");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            let file = global.join("stream-rules.json");
            std::fs::write(&file, serde_json::to_vec(&json!({"version":1,"rules":[rule_fixture()]})).expect("serialize rules")).expect("write rules");
            agent.run("after configuration", |_| {}).await.expect("reloaded turn");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
            std::fs::write(&file, "{ broken").expect("invalid replacement");
            assert!(agent.run("after invalid edit", |_| {}).await.is_err());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        });
    }

    #[test]
    fn stream_rule_source_reloads_at_authored_follow_up_boundaries() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace");
            let global = temp.path().join("global");
            std::fs::create_dir_all(&global).expect("global directory");
            let file = global.join("stream-rules.json");
            let rules = serde_json::to_vec(&json!({"version": 1, "rules": [rule_fixture()]}))
                .expect("serialize rules");
            let provider = Arc::new(RuleScriptProvider::new(vec![
                rule_done("Initial response."),
                rule_done("FORBIDDEN"),
                rule_done("FORBIDDEN"),
                rule_done("Completed."),
            ]));
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent
                .set_stream_rule_source(crate::stream_rules::StreamRuleSource::new(
                    temp.path().to_path_buf(),
                    global,
                    false,
                ))
                .expect("source");
            agent.set_queue_modes(QueueMode::All, QueueMode::OneAtATime);
            agent
                .message_queue
                .push_follow_up(QueuedAgentMessage::generated(user_message("generated notice")));
            agent.queue_follow_up(user_message("authored follow-up"));

            let result = agent
                .run("initial prompt", move |event| {
                    if matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }) {
                        std::fs::write(&file, &rules).expect("edit rules during the run");
                    }
                })
                .await
                .expect("queued turns");
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
            assert_eq!(agent.stream_rule_turn, 2, "only authored input renews the cap");
            let requests = provider.requests.lock().expect("requests");
            assert_user_text(requests[1].last().expect("notice"), "generated notice");
            assert_user_text(requests[2].last().expect("follow-up"), "authored follow-up");
            assert!(requests[3].iter().any(|message| matches!(message,
                Message::Custom(custom) if custom.custom_type == crate::stream_rules::TTSR_CUSTOM_ENTRY_TYPE
            )));
            drop(requests);
            assert!(agent.message_queue.follow_up.is_empty());
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn invalid_stream_rule_reload_preserves_queued_follow_ups_for_recovery() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("workspace");
            let global = temp.path().join("global");
            std::fs::create_dir_all(&global).expect("global directory");
            let file = global.join("stream-rules.json");
            let provider = Arc::new(RuleScriptProvider::new(vec![
                rule_done("Initial response."),
                rule_done("FORBIDDEN"),
                rule_done("Queued work completed."),
            ]));
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent
                .set_stream_rule_source(crate::stream_rules::StreamRuleSource::new(
                    temp.path().to_path_buf(),
                    global,
                    false,
                ))
                .expect("source");
            agent.set_queue_modes(QueueMode::All, QueueMode::All);
            for text in ["first queued task", "second queued task"] {
                let delivery = queued_user_message(text);
                let _ = delivery.bind_persistence_identity(None);
                agent.message_queue.push_follow_up(delivery);
            }
            let accepted = agent.message_queue.follow_up.clone();
            let events = Arc::new(StdMutex::new(Vec::new()));
            let captured = Arc::clone(&events);
            let edited_file = file.clone();
            let result = agent
                .run("initial prompt", move |event| {
                    if matches!(&event, AgentEvent::TurnEnd { turn_index: 0, .. }) {
                        std::fs::write(&edited_file, "{ broken")
                            .expect("invalid edit during the run");
                    }
                    captured.lock().expect("capture events").push(event);
                })
                .await;
            assert!(result.is_err());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(agent.stream_rule_turn, 1);
            assert_eq!(agent.messages().len(), 2, "queued input was not delivered");
            assert_eq!(agent.message_queue.follow_up.len(), accepted.len());
            for (pending, original) in agent.message_queue.follow_up.iter().zip(&accepted) {
                assert_eq!(pending.seq, original.seq);
                assert_eq!(pending.enqueued_at, original.enqueued_at);
                assert_eq!(pending.job_owner_session_id, original.job_owner_session_id);
                assert_eq!(
                    pending.delivery.keyword_scan_source(),
                    original.delivery.keyword_scan_source()
                );
                assert!(
                    pending
                        .delivery
                        .shares_persistence_identity(&original.delivery)
                );
            }
            {
                let events = events.lock().expect("events");
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::TurnStart { .. }))
                        .count(),
                    1
                );
                let ends = events
                    .iter()
                    .filter_map(|event| match event {
                        AgentEvent::AgentEnd {
                            messages, error, ..
                        } => Some((messages, error)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(ends.len(), 1);
                assert_eq!(ends[0].0.len(), 2);
                assert!(ends[0].1.is_some());
                drop(events);
            }

            let rules = serde_json::to_vec(&json!({"version": 1, "rules": [rule_fixture()]}))
                .expect("serialize rules");
            std::fs::write(&file, rules).expect("repair rules");
            let recovered = agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("resume accepted batch after repair");
            assert_eq!(recovered.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
            assert_eq!(agent.stream_rule_turn, 2);
            assert!(agent.message_queue.follow_up.is_empty());
            let users = agent
                .messages()
                .iter()
                .filter_map(|message| match message {
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                users,
                ["initial prompt", "first queued task", "second queued task"]
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    fn invalid_terminal_tool_peer_withholds_the_whole_batch_before_approval() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target.txt");
            let valid = batch_write_call("valid-call", "target.txt", "changed");
            let mut invalid = Vec::new();
            for id in ["", " \t", "valid-call"] {
                invalid.push(ToolCall {
                    id: id.to_string(),
                    ..valid.clone()
                });
            }
            for name in ["", " \n"] {
                invalid.push(ToolCall {
                    id: "invalid-peer".to_string(),
                    name: name.to_string(),
                    ..valid.clone()
                });
            }
            for arguments in [
                Value::Null,
                json!(true),
                json!(42),
                json!("private-argument"),
                json!([]),
            ] {
                invalid.push(ToolCall {
                    id: "invalid-peer".to_string(),
                    arguments,
                    ..valid.clone()
                });
            }

            for bad in invalid {
                for reason in [StopReason::Stop, StopReason::ToolUse, StopReason::Length] {
                    std::fs::write(&target, "original").unwrap();
                    let mut message = assistant_message("Retain the partial explanation");
                    message.stop_reason = reason;
                    message.usage.input = 17;
                    message.usage.output = 5;
                    message.usage.total_tokens = 22;
                    message.content.extend([
                        ContentBlock::ToolCall(valid.clone()),
                        ContentBlock::ToolCall(bad.clone()),
                    ]);
                    let original_content = serde_json::to_value(&message.content).unwrap();
                    let provider = Arc::new(TerminalBatchProvider {
                        events: vec![
                            StreamEvent::Start {
                                partial: assistant_message(""),
                            },
                            StreamEvent::ToolCallEnd {
                                content_index: 1,
                                tool_call: valid.clone(),
                            },
                            StreamEvent::Done { reason, message },
                        ],
                        calls: std::sync::atomic::AtomicUsize::new(0),
                    });
                    let approvals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let observed = Arc::clone(&approvals);
                    let mut agent = Agent::new(
                        provider.clone(),
                        ToolRegistry::new(&["write"], temp.path(), None),
                        AgentConfig {
                            tool_approval: Some(Arc::new(move |_| {
                                observed.fetch_add(1, Ordering::SeqCst);
                                Box::pin(async { ToolApprovalDecision::Allow })
                            })),
                            ..AgentConfig::default()
                        },
                    );
                    let events = Arc::new(StdMutex::new(Vec::new()));
                    let captured = Arc::clone(&events);
                    let result = agent
                        .run("Apply the edits", move |event| {
                            captured.lock().unwrap().push(event);
                        })
                        .await
                        .unwrap();

                    assert_eq!(result.stop_reason, StopReason::Error, "{reason:?}/{bad:?}");
                    let error = result.error_message.as_deref().unwrap();
                    assert!(error.contains("PI_TOOL_CALL_PROTOCOL"));
                    assert!(!error.contains("private-argument"));
                    assert_eq!(
                        serde_json::to_value(&result.content).unwrap(),
                        original_content
                    );
                    assert_eq!(
                        (
                            result.usage.input,
                            result.usage.output,
                            result.usage.total_tokens
                        ),
                        (17, 5, 22)
                    );
                    assert_eq!(approvals.load(Ordering::SeqCst), 0);
                    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
                    assert!(
                        !agent
                            .messages()
                            .iter()
                            .any(|m| matches!(m, Message::ToolResult(_)))
                    );
                    let events = events.lock().unwrap();
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                    );
                    let ended = events
                        .iter()
                        .find_map(|event| match event {
                            AgentEvent::MessageEnd {
                                message: Message::Assistant(message),
                            } => Some(message),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(ended).unwrap(),
                        serde_json::to_value(&result).unwrap()
                    );
                }
            }
        });
    }

    #[test]
    fn valid_parallel_tool_batch_executes_with_stop_tool_use_and_length() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for reason in [StopReason::Stop, StopReason::ToolUse, StopReason::Length] {
                let temp = tempfile::tempdir().unwrap();
                let mut message = assistant_message("Apply both edits");
                message.stop_reason = reason;
                message.content.extend([
                    ContentBlock::ToolCall(batch_write_call("first", "first.txt", "one")),
                    ContentBlock::ToolCall(batch_write_call("second", "second.txt", "two")),
                ]);
                let original = serde_json::to_value(&message).unwrap();
                let provider = Arc::new(TerminalBatchProvider {
                    events: vec![StreamEvent::Done { reason, message }],
                    calls: std::sync::atomic::AtomicUsize::new(0),
                });
                let mut agent = Agent::new(
                    provider.clone(),
                    ToolRegistry::new(&["write"], temp.path(), None),
                    AgentConfig::default(),
                );
                let result = agent.run("Apply the edits", |_| {}).await.unwrap();
                assert_eq!(result.stop_reason, StopReason::Stop);
                assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("first.txt")).unwrap(),
                    "one"
                );
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("second.txt")).unwrap(),
                    "two"
                );
                let Message::Assistant(stored) = &agent.messages()[1] else {
                    panic!("validated assistant batch stays in history");
                };
                assert_eq!(serde_json::to_value(stored).unwrap(), original);
                let results: Vec<_> = agent
                    .messages()
                    .iter()
                    .filter_map(|message| match message {
                        Message::ToolResult(result) => {
                            Some((result.tool_call_id.as_str(), result.is_error))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(results, [("first", false), ("second", false)]);
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn length_tool_batch_withholds_completed_peer_when_another_call_disappears() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for omit_started_peer in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let target = temp.path().join("target.txt");
                std::fs::write(&target, "original").unwrap();
                let completed = batch_write_call("complete", "target.txt", "changed");
                let mut message = assistant_message("Apply the edits");
                message.stop_reason = StopReason::Length;
                message
                    .content
                    .push(ContentBlock::ToolCall(completed.clone()));
                let mut events = vec![StreamEvent::Start {
                    partial: assistant_message(""),
                }];
                // Start, delta, and repeated end updates at the same index
                // represent one call, not four separate batch members.
                events.extend([
                    StreamEvent::ToolCallStart {
                        content_index: 1,
                        id: completed.id.clone(),
                        name: completed.name.clone(),
                    },
                    StreamEvent::ToolCallDelta {
                        content_index: 1,
                        delta: serde_json::to_string(&completed.arguments).unwrap(),
                    },
                    StreamEvent::ToolCallEnd {
                        content_index: 1,
                        tool_call: completed.clone(),
                    },
                    StreamEvent::ToolCallEnd {
                        content_index: 1,
                        tool_call: completed,
                    },
                ]);
                if omit_started_peer {
                    events.extend([
                        StreamEvent::ToolCallStart {
                            content_index: 2,
                            id: "unfinished".to_string(),
                            name: "write".to_string(),
                        },
                        StreamEvent::ToolCallDelta {
                            content_index: 2,
                            delta: "{\"path\":\"other.txt\",\"content\":\"".to_string(),
                        },
                    ]);
                }
                events.push(StreamEvent::Done {
                    reason: StopReason::Length,
                    message,
                });
                let provider = Arc::new(TerminalBatchProvider {
                    events,
                    calls: std::sync::atomic::AtomicUsize::new(0),
                });
                let approvals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let observed = Arc::clone(&approvals);
                let mut agent = Agent::new(
                    provider.clone(),
                    ToolRegistry::new(&["write"], temp.path(), None),
                    AgentConfig {
                        tool_approval: Some(Arc::new(move |_| {
                            observed.fetch_add(1, Ordering::SeqCst);
                            Box::pin(async { ToolApprovalDecision::Allow })
                        })),
                        ..AgentConfig::default()
                    },
                );
                let result = agent.run("Apply the edits", |_| {}).await.unwrap();
                if omit_started_peer {
                    assert_eq!(result.stop_reason, StopReason::Error);
                    assert_eq!(
                        result.error_message.as_deref(),
                        Some(TRUNCATED_TOOL_CALL_ERROR)
                    );
                    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                    assert_eq!(approvals.load(Ordering::SeqCst), 0);
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
                    assert!(
                        !agent
                            .messages()
                            .iter()
                            .any(|m| matches!(m, Message::ToolResult(_)))
                    );
                } else {
                    assert_eq!(result.stop_reason, StopReason::Stop);
                    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
                    assert_eq!(approvals.load(Ordering::SeqCst), 1);
                    assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed");
                }
                assert!(!temp.path().join("other.txt").exists());
            }
        });
    }

    #[test]
    fn terminal_failure_envelopes_never_execute_stale_success_snapshots() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target.txt");
            for reason in [StopReason::Error, StopReason::Aborted] {
                for error_envelope in [false, true] {
                    let mut message = assistant_message("Partial provider answer");
                    message.error_message = Some("original provider diagnostic".to_string());
                    message
                        .content
                        .push(ContentBlock::ToolCall(batch_write_call(
                            "call",
                            "target.txt",
                            "must not execute",
                        )));
                    let event = if error_envelope {
                        StreamEvent::Error {
                            reason,
                            error: message,
                        }
                    } else {
                        StreamEvent::Done { reason, message }
                    };
                    let provider = Arc::new(TerminalBatchProvider {
                        events: vec![event],
                        calls: std::sync::atomic::AtomicUsize::new(0),
                    });
                    let mut agent = Agent::new(
                        provider,
                        ToolRegistry::new(&["write"], temp.path(), None),
                        AgentConfig::default(),
                    );
                    let result = agent.run("Apply the edit", |_| {}).await.unwrap();
                    assert_eq!(result.stop_reason, reason);
                    assert_eq!(
                        result.error_message.as_deref(),
                        Some("original provider diagnostic")
                    );
                    assert!(!target.exists(), "a terminal failure cannot create a file");
                    assert!(
                        !agent
                            .messages()
                            .iter()
                            .any(|m| matches!(m, Message::ToolResult(_)))
                    );
                }
            }
        });
    }

    #[test]
    fn terminal_length_envelope_rejects_a_dropped_incomplete_call() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for snapshot_reason in [StopReason::Stop, StopReason::ToolUse] {
                let mut message = assistant_message("Retain the partial explanation");
                message.stop_reason = snapshot_reason;
                message.usage.input = 9;
                message.usage.output = 2;
                message.usage.total_tokens = 11;
                let original_content = serde_json::to_value(&message.content).unwrap();
                let provider = Arc::new(TerminalBatchProvider {
                    events: vec![
                        StreamEvent::Start {
                            partial: assistant_message(""),
                        },
                        StreamEvent::ToolCallStart {
                            content_index: 1,
                            id: "partial-call".to_string(),
                            name: "write".to_string(),
                        },
                        StreamEvent::ToolCallDelta {
                            content_index: 1,
                            delta: "{\"path\":\"target.txt\",\"content\":\"".to_string(),
                        },
                        // The provider omits the unfinished call from its
                        // snapshot while the envelope still reports Length.
                        StreamEvent::Done {
                            reason: StopReason::Length,
                            message,
                        },
                    ],
                    calls: std::sync::atomic::AtomicUsize::new(0),
                });
                let temp = tempfile::tempdir().unwrap();
                let mut agent = Agent::new(
                    provider.clone(),
                    ToolRegistry::new(&["write"], temp.path(), None),
                    AgentConfig::default(),
                );
                let result = agent.run("Apply the edit", |_| {}).await.unwrap();
                assert_eq!(result.stop_reason, StopReason::Error);
                assert_eq!(
                    result.error_message.as_deref(),
                    Some(TRUNCATED_TOOL_CALL_ERROR)
                );
                assert_eq!(
                    serde_json::to_value(&result.content).unwrap(),
                    original_content
                );
                assert_eq!(result.usage.total_tokens, 11);
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert!(!temp.path().join("target.txt").exists());
            }
        });
    }

    #[test]
    fn terminal_pause_envelope_continues_signed_calls_without_local_execution() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target.txt");
            let mut message = assistant_message("The server tool is still working");
            message
                .content
                .push(ContentBlock::ToolCall(batch_write_call(
                    "server-call",
                    "target.txt",
                    "must not execute",
                )));
            let original_content = serde_json::to_value(&message.content).unwrap();
            let provider = Arc::new(TerminalBatchProvider {
                events: vec![StreamEvent::Done {
                    reason: StopReason::PauseTurn,
                    message,
                }],
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut agent = Agent::new(
                provider.clone(),
                ToolRegistry::new(&["write"], temp.path(), None),
                AgentConfig::default(),
            );
            let result = agent.run("Apply the edit", |_| {}).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
            assert!(
                !target.exists(),
                "server tool pauses cannot execute locally"
            );
            assert!(
                !agent
                    .messages()
                    .iter()
                    .any(|m| matches!(m, Message::ToolResult(_)))
            );
            let Message::Assistant(stored) = &agent.messages()[1] else {
                panic!("paused assistant stays in history for continuation");
            };
            assert_eq!(stored.stop_reason, StopReason::PauseTurn);
            assert_eq!(
                serde_json::to_value(&stored.content).unwrap(),
                original_content
            );
        });
    }

    #[test]
    fn terminal_refusal_preserves_provider_details_without_local_execution() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target.txt");
            for (snapshot_reason, reason) in [
                (StopReason::Refusal, StopReason::Refusal),
                (StopReason::Stop, StopReason::Refusal),
                (StopReason::Refusal, StopReason::Stop),
                (StopReason::Refusal, StopReason::PauseTurn),
            ] {
                let mut message = assistant_message("Provider declined the request");
                message.stop_reason = snapshot_reason;
                message.stop_details = Some(crate::model::StopDetails {
                    kind: "refusal".to_string(),
                    category: Some("provider-policy".to_string()),
                    explanation: Some("original provider explanation".to_string()),
                });
                message
                    .content
                    .push(ContentBlock::ToolCall(batch_write_call(
                        "refused-call",
                        "target.txt",
                        "must not execute",
                    )));
                let mut expected = message.clone();
                expected.stop_reason = StopReason::Refusal;
                let provider = Arc::new(TerminalBatchProvider {
                    events: vec![StreamEvent::Done { reason, message }],
                    calls: std::sync::atomic::AtomicUsize::new(0),
                });
                let mut agent = Agent::new(
                    provider.clone(),
                    ToolRegistry::new(&["write"], temp.path(), None),
                    AgentConfig::default(),
                );
                let result = agent.run("Apply the edit", |_| {}).await.unwrap();
                assert_eq!(
                    serde_json::to_value(&result).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert!(!target.exists(), "refused requests cannot execute locally");
                assert!(
                    !agent
                        .messages()
                        .iter()
                        .any(|m| matches!(m, Message::ToolResult(_)))
                );
            }
        });
    }

    #[test]
    fn terminal_validation_preserves_failed_aborted_and_signed_pause_payloads() {
        for reason in [
            StopReason::Error,
            StopReason::Aborted,
            StopReason::PauseTurn,
            StopReason::Refusal,
        ] {
            let mut message = assistant_message("provider explanation");
            message.stop_reason = reason;
            message.error_message = Some("original diagnostic".to_string());
            message.content.extend([
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "signed reasoning".to_string(),
                    thinking_signature: Some("opaque-signature".to_string()),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: Value::Null,
                    thought_signature: Some("opaque-call".to_string()),
                }),
            ]);
            let original = serde_json::to_value(&message).unwrap();
            let mut agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let on_event: AgentEventHandler = Arc::new(|_| {});
            let AssistantStreamAttempt::Complete(result) =
                agent.finalize_assistant_message(message, &on_event, false)
            else {
                panic!("finalization never reports a stream-rule interruption");
            };
            assert_eq!(serde_json::to_value(result).unwrap(), original);
        }
    }

    /// #148: first turn is cut off by the token cap while a tool call is still
    /// streaming, exactly as OpenAI (`finish_reason: "length"`) and Anthropic
    /// (`stop_reason: "max_tokens"`) report it — the tool call keeps its name
    /// but its argument fragment fails to parse, so the provider stores
    /// `arguments: null`. A second turn is scripted so the pre-fix behaviour
    /// (continue as if nothing went wrong) is observable rather than hanging.
    #[derive(Debug)]
    struct TruncatedToolCallProvider {
        stream_calls: StdArc<std::sync::atomic::AtomicUsize>,
    }

    impl TruncatedToolCallProvider {
        fn new() -> Self {
            Self {
                stream_calls: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        /// Shared handle to the stream-call counter, taken before the provider
        /// is moved into the agent.
        fn calls(&self) -> StdArc<std::sync::atomic::AtomicUsize> {
            StdArc::clone(&self.stream_calls)
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TruncatedToolCallProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            if call_index > 0 {
                let events = vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_message("recovered"),
                })];
                return Ok(Box::pin(futures::stream::iter(events)));
            }

            let mut truncated = assistant_message("Let me read that file");
            truncated.stop_reason = StopReason::Length;
            truncated.content.push(ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                // `{"path": "/etc/ho` does not parse; both providers fall back
                // to null after logging a parse warning.
                arguments: Value::Null,
                thought_signature: None,
            }));

            let events = vec![
                Ok(StreamEvent::Start {
                    partial: assistant_message(""),
                }),
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "Let me read that file".to_string(),
                }),
                Ok(StreamEvent::ToolCallStart {
                    content_index: 1,
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    content_index: 1,
                    delta: "{\"path\": \"/etc/ho".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Length,
                    message: truncated,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// #148 negative case: the same token cap, but the model was writing prose.
    /// No tool call was ever started, so this is an ordinary `Length` stop and
    /// must not be rewritten into an error.
    #[derive(Debug)]
    struct TruncatedTextProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TruncatedTextProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let mut truncated = assistant_message("The answer is fourty-tw");
            truncated.stop_reason = StopReason::Length;

            let events = vec![
                Ok(StreamEvent::Start {
                    partial: assistant_message(""),
                }),
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "The answer is fourty-tw".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Length,
                    message: truncated,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn truncated_tool_call_finalizes_as_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = TruncatedToolCallProvider::new();
            let stream_calls = provider.calls();
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

            let result = agent
                .run_with_message_with_abort(user_message("read /etc/hosts"), None, |_| {})
                .await
                .expect("run");

            assert_eq!(
                result.stop_reason,
                StopReason::Error,
                "truncated tool call must not finalize as a normal turn"
            );
            let error_message = result
                .error_message
                .as_deref()
                .expect("truncated turn carries an error message");
            assert!(
                error_message.contains("truncated before tool call completed"),
                "unexpected error message: {error_message}"
            );

            // The turn must stop here: continuing would either execute a tool
            // call whose arguments were cut off, or silently drop it.
            assert_eq!(
                stream_calls.load(Ordering::SeqCst),
                1,
                "agent kept running past the truncated tool call"
            );

            // The partial text is retained so the caller can show what arrived.
            let last = agent
                .messages()
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::Assistant(assistant) => Some(Arc::clone(assistant)),
                    _ => None,
                })
                .expect("assistant message in history");
            assert_eq!(last.stop_reason, StopReason::Error);
            assert!(
                last.content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(text)
                        if text.text == "Let me read that file")),
                "partial text was dropped: {:?}",
                last.content
            );
        });
    }

    #[test]
    fn truncated_text_without_tool_call_stays_a_length_stop() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(TruncatedTextProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(provider, tools, AgentConfig::default());

            let result = agent
                .run_with_message_with_abort(user_message("what is the answer"), None, |_| {})
                .await
                .expect("run");

            assert_eq!(
                result.stop_reason,
                StopReason::Length,
                "a plain length stop must not be rewritten into an error"
            );
            assert_eq!(result.error_message, None);
            assert!(
                result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(text)
                        if text.text == "The answer is fourty-tw")),
                "partial text was dropped: {:?}",
                result.content
            );
        });
    }

    #[test]
    fn semantic_context_bundle_injection_is_disabled_by_default() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .run_text("hello".to_string(), |_| {})
                .await
                .expect("run with default context settings");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(recorded_calls[0].messages.len(), 1);
            assert_user_text(&recorded_calls[0].messages[0], "hello");
            assert!(recorded_calls[0].system_prompt.is_none());
        });
    }

    #[test]
    fn queued_steering_and_follow_up_messages_apply_magic_keywords_on_delivery() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.queue_steering(user_message("ultrathink before continuing"));
            agent.queue_follow_up(user_message("orchestrate the follow-up"));

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("queued messages complete");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(
                recorded_calls.len(),
                2,
                "follow-up must trigger a second provider turn"
            );
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::High),
                "queued steering ultrathink must affect its first outbound request"
            );
            assert!(
                recorded_calls[0]
                    .system_prompt
                    .as_deref()
                    .is_none_or(|prompt| !prompt.contains("invoked `orchestrate`")),
                "the later follow-up directive must not leak into the first request"
            );
            assert!(
                recorded_calls[1]
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("invoked `orchestrate`")),
                "queued follow-up orchestrate must affect its outbound request"
            );

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 2);
            assert_eq!(activations[0].word, "ultrathink");
            assert_eq!(activations[1].word, "orchestrate");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn continuation_follow_up_first_precedes_provider_and_later_steering() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let queued_steering = Arc::new(StdTestMutex::new(Some(
                QueuedAgentMessage::from_authored_message(user_message("steer after handoff")),
            )));
            let steering_fetcher_state = Arc::clone(&queued_steering);
            let steering_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let steering_fetcher_state = Arc::clone(&steering_fetcher_state);
                    Box::pin(async move {
                        steering_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };

            let first_visible = "expanded payload without a magic word";
            let second_visible = "generated ultrathink payload";
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(vec![
                QueuedAgentMessage::authored(
                    user_message(first_visible),
                    "please orchestrate this follow-up",
                ),
                QueuedAgentMessage::generated(user_message(second_visible)),
            ])));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(steering_fetcher)), None);
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));
            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("follow-up-first continuation");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(
                recorded_calls.len(),
                2,
                "initial follow-up and later steering must each drive one provider request"
            );
            assert_eq!(recorded_calls[0].messages.len(), 2);
            assert_user_text(&recorded_calls[0].messages[0], first_visible);
            assert_user_text(&recorded_calls[0].messages[1], second_visible);
            assert!(
                recorded_calls[0].messages.iter().all(|message| {
                    !matches!(
                        message,
                        Message::User(UserMessage { content: UserContent::Text(text), .. })
                            if text == "steer after handoff"
                    )
                }),
                "steering admitted for the resumed turn must not precede its initial follow-up"
            );
            assert!(recorded_calls[1].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage { content: UserContent::Text(text), .. })
                        if text == "steer after handoff"
                )
            }));
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "generated ultrathink bytes must remain inert"
            );
            assert!(
                recorded_calls[0]
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("invoked `orchestrate`")),
                "the exact authored source must activate its directive"
            );
        });
    }

    #[test]
    fn continuation_preflights_owning_source_before_consuming_staged_follow_up() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);
            agent.queue_follow_up(user_message("older staged follow-up"));

            let owning_source_fetched = StdArc::new(AtomicBool::new(false));
            let fetched_for_source = StdArc::clone(&owning_source_fetched);
            let queued_owning = StdArc::new(StdTestMutex::new(Some(queued_user_message(
                "new owning follow-up",
            ))));
            let queued_for_source = StdArc::clone(&queued_owning);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    fetched_for_source.store(true, Ordering::SeqCst);
                    let queued_for_source = StdArc::clone(&queued_for_source);
                    Box::pin(async move {
                        queued_for_source
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            let events = StdArc::new(StdTestMutex::new(Vec::new()));
            let events_for_failed_preflight = StdArc::clone(&events);
            let result = agent
                .run_continue_with_follow_up_on_ready_with_abort(
                    None,
                    || false,
                    move |event| {
                        events_for_failed_preflight
                            .lock()
                            .expect("event capture")
                            .push(event);
                    },
                )
                .await;
            assert!(result.is_err(), "failed source preflight must stop the run");
            assert!(
                owning_source_fetched.load(Ordering::SeqCst),
                "an older staged batch must not bypass the owning fetcher"
            );
            assert!(events.lock().expect("event capture").is_empty());
            assert!(calls.lock().expect("provider calls").is_empty());
            assert_eq!(agent.message_queue.follow_up.len(), 2);
            assert_user_text(
                &agent.message_queue.follow_up[0].delivery.message,
                "older staged follow-up",
            );
            assert_user_text(
                &agent.message_queue.follow_up[1].delivery.message,
                "new owning follow-up",
            );

            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("resume retained follow-ups after successful preflight");
            let recorded_calls = calls.lock().expect("provider calls").clone();
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(recorded_calls[0].messages.len(), 2);
            assert_user_text(&recorded_calls[0].messages[0], "older staged follow-up");
            assert_user_text(&recorded_calls[0].messages[1], "new owning follow-up");
        });
    }

    #[test]
    fn max_time_restores_undelivered_steering_for_next_production_run() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    max_time: Some(std::time::Duration::ZERO),
                    ..AgentConfig::default()
                },
            );
            agent.set_queue_modes(QueueMode::OneAtATime, QueueMode::All);
            agent.queue_steering(user_message("accepted steering one"));
            agent.queue_steering(user_message("accepted steering two"));
            agent.queue_steering(user_message("accepted steering three"));

            let events = StdArc::new(StdTestMutex::new(Vec::new()));
            let events_for_capped_run = StdArc::clone(&events);
            let capped = agent
                .run_with_message_with_abort(user_message("initial prompt"), None, move |event| {
                    events_for_capped_run
                        .lock()
                        .expect("event capture")
                        .push(event);
                })
                .await
                .expect("time-capped run");
            assert!(assistant_text_content(&capped.content).contains("time cap reached"));
            assert!(calls.lock().expect("provider calls").is_empty());
            assert_eq!(agent.message_queue.steering.len(), 3);
            assert_user_text(
                &agent.message_queue.steering[0].delivery.message,
                "accepted steering one",
            );
            assert_user_text(
                &agent.message_queue.steering[1].delivery.message,
                "accepted steering two",
            );
            assert_user_text(
                &agent.message_queue.steering[2].delivery.message,
                "accepted steering three",
            );
            let captured_events = { events.lock().expect("event capture").clone() };
            assert_eq!(
                captured_events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentStart { .. }))
                    .count(),
                1
            );
            assert_eq!(
                captured_events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                    .count(),
                1
            );
            assert!(captured_events.iter().all(|event| !matches!(
                event,
                AgentEvent::TurnStart { .. } | AgentEvent::TurnEnd { .. }
            )));

            agent.config.max_time = None;
            agent.set_queue_modes(QueueMode::All, QueueMode::All);
            agent
                .run_with_message_with_abort(user_message("resume prompt"), None, |_| {})
                .await
                .expect("next production run");
            assert!(agent.message_queue.steering.is_empty());
            let recorded_calls = calls.lock().expect("provider calls").clone();
            assert_eq!(recorded_calls.len(), 1);
            let user_texts = recorded_calls[0]
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                user_texts,
                vec![
                    "initial prompt",
                    "resume prompt",
                    "accepted steering one",
                    "accepted steering two",
                    "accepted steering three"
                ]
            );
        });
    }

    #[test]
    fn continuation_follow_up_first_preserves_full_admitted_batch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("admitted follow-up {index}"))
                .collect::<Vec<_>>();
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("full admitted follow-up batch");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(recorded_calls[0].messages.len(), expected.len());
            for (message, expected_text) in recorded_calls[0].messages.iter().zip(&expected) {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    fn owning_steering_fetcher_preserves_full_admitted_batch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("admitted steering {index}"))
                .collect::<Vec<_>>();
            let queued = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let fetcher_state = Arc::clone(&queued);
            let fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let fetcher_state = Arc::clone(&fetcher_state);
                    Box::pin(async move {
                        fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(fetcher)), None);

            agent
                .run_with_message_with_abort(user_message("initial turn"), None, |_| {})
                .await
                .expect("full admitted steering batch");

            let recorded_calls = match calls.lock() {
                Ok(calls) => calls.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(recorded_calls[0].messages.len(), expected.len() + 1);
            assert_user_text(&recorded_calls[0].messages[0], "initial turn");
            for (message, expected_text) in recorded_calls[0].messages[1..].iter().zip(&expected) {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    fn ordinary_idle_boundary_preserves_full_primary_follow_up_batch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("ordinary follow-up {index}"))
                .collect::<Vec<_>>();
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            agent
                .run_with_message_with_abort(user_message("initial turn"), None, |_| {})
                .await
                .expect("ordinary follow-up delivery");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 2);
            let second_call = &recorded_calls[1].messages;
            assert!(second_call.len() >= expected.len());
            let delivered = &second_call[second_call.len() - expected.len()..];
            for (message, expected_text) in delivered.iter().zip(&expected) {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn ordinary_follow_up_batch_remains_staged_and_resumes_after_max_time() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    max_time: Some(std::time::Duration::from_millis(100)),
                    ..AgentConfig::default()
                },
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("staged follow-up {index}"))
                .collect::<Vec<_>>();
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            let events = StdArc::new(StdTestMutex::new(Vec::new()));
            let events_for_run = StdArc::clone(&events);
            let result = agent
                .run_with_message_with_abort(user_message("initial turn"), None, move |event| {
                    events_for_run.lock().expect("event capture").push(event);
                })
                .await
                .expect("max-time stop");
            assert!(
                assistant_text_content(&result.content).contains("time cap reached"),
                "the returned terminal message must report the cap"
            );

            let calls_len = match calls.lock() {
                Ok(calls) => calls.len(),
                Err(poisoned) => poisoned.into_inner().len(),
            };
            assert_eq!(
                calls_len, 1,
                "the expired time cap must prevent a follow-up provider turn"
            );
            assert_eq!(
                agent.message_queue.follow_up.len(),
                128,
                "the accepted batch must remain staged for a later run"
            );
            for (entry, expected_text) in agent.message_queue.follow_up.iter().zip(&expected) {
                assert_user_text(&entry.delivery.message, expected_text);
            }

            let captured_events = { events.lock().expect("event capture").clone() };
            assert_eq!(
                captured_events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentStart { .. }))
                    .count(),
                1
            );
            let agent_ends = captured_events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::AgentEnd {
                        messages, error, ..
                    } => Some((messages, error)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                agent_ends.len(),
                1,
                "AgentStart must have one matching AgentEnd"
            );
            assert!(agent_ends[0].1.is_none());
            assert!(agent_ends[0].0.iter().any(|message| {
                matches!(
                    message,
                    Message::Assistant(assistant)
                        if assistant_text_content(&assistant.content).contains("time cap reached")
                )
            }));

            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("resume staged follow-up batch");
            assert!(agent.message_queue.follow_up.is_empty());
            let recorded_calls = match calls.lock() {
                Ok(calls) => calls.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            assert_eq!(recorded_calls.len(), 2);
            let resumed = &recorded_calls[1].messages;
            assert!(resumed.len() >= expected.len());
            for (message, expected_text) in resumed[resumed.len() - expected.len()..]
                .iter()
                .zip(&expected)
            {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    fn fetched_message_scans_only_explicit_authored_source() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);

            let expanded = "generated ultrathink and workflowz bytes".to_string();
            let queued = Arc::new(StdTestMutex::new(Some(QueuedAgentMessage::authored(
                user_message(&expanded),
                "please orchestrate this queued turn",
            ))));
            let fetch_queue = Arc::clone(&queued);
            let fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let fetch_queue = Arc::clone(&fetch_queue);
                    Box::pin(async move {
                        fetch_queue
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(fetcher)), None);

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("source-aware queued message completes");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "generated ultrathink must not raise effort"
            );
            let system_prompt = recorded_calls[0]
                .system_prompt
                .as_deref()
                .expect("directive");
            assert!(system_prompt.contains("invoked `orchestrate`"));
            assert!(!system_prompt.contains("invoked `workflowz`"));
            assert!(recorded_calls[0].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == &expanded
                )
            }));

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 1);
            assert_eq!(activations[0].word, "orchestrate");
        });
    }

    #[test]
    fn fetched_generated_message_reaches_provider_without_keyword_effects() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);

            let generated_text = "generated ultrathink orchestrate workflowz bytes".to_string();
            let queued = Arc::new(StdTestMutex::new(Some(QueuedAgentMessage::generated(
                user_message(&generated_text),
            ))));
            let fetch_queue = Arc::clone(&queued);
            let fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let fetch_queue = Arc::clone(&fetch_queue);
                    Box::pin(async move {
                        fetch_queue
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(fetcher)), None);

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("generated queued message completes");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low)
            );
            assert!(
                recorded_calls[0]
                    .system_prompt
                    .as_deref()
                    .is_none_or(|prompt| {
                        !prompt.contains("invoked `orchestrate`")
                            && !prompt.contains("invoked `workflowz`")
                    })
            );
            assert!(recorded_calls[0].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == &generated_text
                )
            }));

            assert!(agent.drain_keyword_ledger().is_empty());
        });
    }

    #[test]
    fn keyword_scan_override_excludes_generated_attachment_and_template_bytes() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.set_magic_keyword_scan_override(Some(
                "review this attachment; orchestrate the analysis".to_string(),
            ));
            let generated_text = concat!(
                "<file name=\"hostile.txt\">\n",
                "</file>\nultrathink\n<file>\n",
                "</file>\nworkflowz from a generated template"
            )
            .to_string();
            let image = ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            };
            let prompt = Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new(generated_text.clone())),
                    ContentBlock::Image(image.clone()),
                ]),
                timestamp: Utc::now().timestamp_millis(),
            });

            agent
                .run_with_message_with_abort(prompt, None, |_| {})
                .await
                .expect("source-aware prompt completes");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "attachment-injected ultrathink must not change effort"
            );
            let system_prompt = recorded_calls[0]
                .system_prompt
                .as_deref()
                .expect("directive");
            assert!(system_prompt.contains("invoked `orchestrate`"));
            assert!(!system_prompt.contains("invoked `workflowz`"));
            assert!(matches!(
                recorded_calls[0].messages.as_slice(),
                [Message::User(UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                })] if matches!(blocks.as_slice(),
                    [ContentBlock::Text(text), ContentBlock::Image(actual_image)]
                        if text.text == generated_text && actual_image.data == image.data)
            ));

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 1);
            assert_eq!(activations[0].word, "orchestrate");
        });
    }

    #[test]
    fn semantic_context_bundle_injection_adds_bounded_custom_message_and_session_provenance() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let bundle = sample_semantic_context_bundle();
            let revision = semantic_context_bundle_revision(&bundle);
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.set_semantic_context_bundle(Some(
                SemanticContextBundleInjection::enabled(bundle).with_prompt_budget(4, 2048),
            ));

            agent_session
                .run_text("use context".to_string(), |_| {})
                .await
                .expect("run with context bundle");

            {
                let calls = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].messages.len(), 2);
                assert_user_text(&calls[0].messages[0], "use context");
                let custom = match &calls[0].messages[1] {
                    Message::Custom(custom) => custom,
                    other => {
                        assert!(
                            matches!(other, Message::Custom(_)),
                            "expected custom semantic context message"
                        );
                        return;
                    }
                };
                assert_eq!(custom.custom_type, SEMANTIC_CONTEXT_CUSTOM_TYPE);
                assert!(custom.display);
                assert!(custom.content.len() <= 2048);
                assert!(custom.content.contains("Semantic Context Bundle"));
                assert!(custom.content.contains("src/agent.rs"));
                let details = custom.details.as_ref().expect("context provenance");
                assert_eq!(
                    details.get("bundleRevision").and_then(Value::as_str),
                    Some(revision.as_str())
                );
                assert_eq!(
                    details
                        .pointer("/provider/promptShape")
                        .and_then(Value::as_str),
                    Some("custom_user_message")
                );
                drop(calls);
            }

            let cx = crate::agent_cx::AgentCx::for_request();
            let stored = session
                .lock(cx.cx())
                .await
                .expect("session lock")
                .to_messages_for_current_path();
            assert!(
                stored.iter().any(|message| matches!(
                    message,
                    Message::Custom(CustomMessage { custom_type, details, display: true, .. })
                        if custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                            && details
                                .as_ref()
                                .and_then(|value| value.get("bundleRevision"))
                                .and_then(Value::as_str)
                                == Some(revision.as_str())
                )),
                "semantic context provenance was not persisted in session messages: {stored:?}"
            );
        });
    }

    #[test]
    fn semantic_context_bundle_uses_system_prompt_append_for_providers_without_custom_context() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let bundle = sample_semantic_context_bundle();
            let revision = semantic_context_bundle_revision(&bundle);
            let provider = CapturingProvider::new("gitlab-chat");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    system_prompt: Some("base prompt".to_string()),
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.set_semantic_context_bundle(Some(
                SemanticContextBundleInjection::enabled(bundle).with_prompt_budget(4, 2048),
            ));

            agent_session
                .run_text("gitlab turn".to_string(), |_| {})
                .await
                .expect("run with system prompt context");

            {
                let calls = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].messages.len(), 1);
                assert_user_text(&calls[0].messages[0], "gitlab turn");
                let system_prompt = calls[0].system_prompt.as_deref().expect("system prompt");
                assert!(system_prompt.contains("base prompt"));
                assert!(system_prompt.contains("Semantic Context Bundle"));
                assert!(system_prompt.contains("src/agent.rs"));
                drop(calls);
            }

            let cx = crate::agent_cx::AgentCx::for_request();
            let stored = session
                .lock(cx.cx())
                .await
                .expect("session lock")
                .to_messages_for_current_path();
            assert!(
                stored.iter().any(|message| matches!(
                    message,
                    Message::Custom(CustomMessage { custom_type, details, display: false, .. })
                        if custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                            && details
                                .as_ref()
                                .and_then(|value| value.get("bundleRevision"))
                                .and_then(Value::as_str)
                                == Some(revision.as_str())
                )),
                "hidden semantic context provenance was not persisted in session messages: {stored:?}"
            );
            assert_eq!(agent_session.agent.system_prompt(), Some("base prompt"));
        });
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    fn semantic_context_continuation_preserves_bundle_across_provider_shapes_without_duplicate_entries()
     {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(Box::pin(async {
            for initial_api in ["gitlab-chat", "test-api"] {
                let bundle = sample_semantic_context_bundle();
                let agent = Agent::new(
                    Arc::new(CapturingProvider::new(initial_api)),
                    ToolRegistry::from_tools(Vec::new()),
                    AgentConfig {
                        system_prompt: Some("base prompt".to_string()),
                        ..AgentConfig::default()
                    },
                );
                let session = Arc::new(Mutex::new(Session::in_memory()));
                let mut agent_session = AgentSession::new(
                    agent,
                    Arc::clone(&session),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                agent_session.set_semantic_context_bundle(Some(
                    SemanticContextBundleInjection::enabled(bundle).with_prompt_budget(4, 2048),
                ));
                agent_session
                    .run_text("first turn".to_string(), |_| {})
                    .await
                    .unwrap();

                let semantic_entries = |stored: &Session| {
                    stored
                        .to_messages_for_current_path()
                        .into_iter()
                        .filter(|message| {
                            matches!(
                                message,
                                Message::Custom(custom)
                                    if custom.custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                            )
                        })
                        .map(|message| serde_json::to_value(message).unwrap())
                        .collect::<Vec<_>>()
                };
                let cx = crate::agent_cx::AgentCx::for_request();
                let original = {
                    let stored = session.lock(cx.cx()).await.unwrap();
                    semantic_entries(&stored)
                };
                assert_eq!(original.len(), 1);

                for target_api in ["bedrock-converse-stream", "test-api", "gitlab-chat"] {
                    let provider = CapturingProvider::new(target_api);
                    let calls = provider.calls();
                    agent_session.agent.set_provider(Arc::new(provider));
                    agent_session
                        .run_continue_with_abort_and_system_prompt(
                            None,
                            Some("hook-system"),
                            |_| {},
                        )
                        .await
                        .unwrap();
                    {
                        let calls = calls.lock().unwrap();
                        assert_eq!(calls.len(), 1);
                        let request = &calls[0];
                        let system = request.system_prompt.as_deref().unwrap();
                        assert!(system.starts_with("hook-system"));
                        let semantic_messages = request
                            .messages
                            .iter()
                            .filter_map(|message| match message {
                                Message::Custom(custom)
                                    if custom.custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE =>
                                {
                                    Some(custom)
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        if target_api == "test-api" {
                            assert_eq!(system, "hook-system");
                            assert_eq!(semantic_messages.len(), 1);
                            assert!(semantic_messages[0].content.contains("src/agent.rs"));
                        } else {
                            assert!(semantic_messages.is_empty());
                            assert_eq!(system.matches("# Semantic Context Bundle").count(), 1);
                            assert!(system.contains("src/agent.rs"));
                        }
                    }
                    assert_eq!(agent_session.agent.system_prompt(), Some("base prompt"));
                    let stored = session.lock(cx.cx()).await.unwrap();
                    assert_eq!(semantic_entries(&stored), original);
                }
            }
        }));
    }

    #[test]
    fn enable_extensions_policy_resolution_defaults_to_permissive() {
        let policy = AgentSession::resolve_extension_policy_for_enable(None, None);
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn enable_extensions_policy_resolution_respects_config_default_toggle() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let policy = AgentSession::resolve_extension_policy_for_enable(Some(&config), None);
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
    }

    #[test]
    fn enable_extensions_policy_resolution_prefers_explicit_policy() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let explicit = crate::extensions::PolicyProfile::Permissive.to_policy();
        let policy =
            AgentSession::resolve_extension_policy_for_enable(Some(&config), Some(explicit));
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn test_extract_tool_calls() {
        let content = vec![
            ContentBlock::Text(TextContent::new("Hello")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "file.txt"}),
                thought_signature: None,
            }),
            ContentBlock::Text(TextContent::new("World")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc2".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }),
        ];

        let tool_calls = extract_tool_calls(&content);
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].name, "read");
        assert_eq!(tool_calls[1].name, "bash");
    }

    #[test]
    fn test_agent_config_default() {
        // Tests don't mutate env (the crate forbids unsafe code, and
        // `std::env::set_var` is unsafe in 2024 edition); under typical
        // `cargo test` invocation `PI_MAX_TOOL_ITERATIONS` is unset, so
        // this assertion holds. If a developer's shell happens to export
        // that var, this test will reflect their effective default — which
        // is the correct behavior, not a bug.
        let config = AgentConfig::default();
        let expected = resolved_max_tool_iterations_default();
        assert_eq!(config.max_tool_iterations, expected);
        assert!(config.system_prompt.is_none());
        assert!(!config.block_images);
    }

    #[test]
    fn resolve_max_tool_iterations_handles_unset_empty_and_whitespace() {
        assert_eq!(
            resolve_max_tool_iterations(None),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("    ")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
    }

    #[test]
    fn resolve_max_tool_iterations_rejects_zero_and_invalid() {
        assert_eq!(
            resolve_max_tool_iterations(Some("0")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("not-a-number")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("-5")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("3.14")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
    }

    #[test]
    fn resolve_max_tool_iterations_accepts_valid_overrides_and_trims_whitespace() {
        assert_eq!(resolve_max_tool_iterations(Some("1")), 1);
        assert_eq!(resolve_max_tool_iterations(Some("100")), 100);
        assert_eq!(resolve_max_tool_iterations(Some("  200  ")), 200);
        assert_eq!(resolve_max_tool_iterations(Some("999")), 999);
    }

    #[test]
    fn resolve_max_tool_iterations_clamps_above_ceiling() {
        assert_eq!(
            resolve_max_tool_iterations(Some("99999")),
            MAX_TOOL_ITERATIONS_CEILING
        );
        // The ceiling itself should pass through unchanged.
        assert_eq!(
            resolve_max_tool_iterations(Some("1000")),
            MAX_TOOL_ITERATIONS_CEILING
        );
    }

    #[test]
    fn clamp_max_tool_iterations_matches_resolve_semantics() {
        // None -> default, 0 -> default (with warning), >ceiling -> ceiling.
        assert_eq!(clamp_max_tool_iterations(None), MAX_TOOL_ITERATIONS_DEFAULT);
        assert_eq!(
            clamp_max_tool_iterations(Some(0)),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(clamp_max_tool_iterations(Some(7)), 7);
        assert_eq!(
            clamp_max_tool_iterations(Some(usize::MAX)),
            MAX_TOOL_ITERATIONS_CEILING
        );
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_for_default_cap() {
        // Default cap = 50; (50 * 4) / 5 = 40 → warn at 40+.
        assert!(!should_warn_at_iteration_threshold(39, 50));
        assert!(should_warn_at_iteration_threshold(40, 50));
        assert!(should_warn_at_iteration_threshold(50, 50));
        // Off-by-one regression guard: not at 39 even with default cap.
        assert!(!should_warn_at_iteration_threshold(0, 50));
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_for_custom_caps() {
        for (cap, threshold) in [(100usize, 80usize), (200, 160), (1000, 800)] {
            assert!(
                !should_warn_at_iteration_threshold(threshold - 1, cap),
                "expected no warning below threshold (current=cap={cap}, threshold={threshold})"
            );
            assert!(
                should_warn_at_iteration_threshold(threshold, cap),
                "expected warning at threshold (cap={cap}, threshold={threshold})"
            );
        }
    }

    #[test]
    fn iteration_warning_skipped_for_caps_below_minimum() {
        // For caps under ITERATION_WARN_MIN_CAP (5), the warning never
        // fires regardless of `current`. This avoids noise on tiny ceilings
        // where the warning would land on iteration 0 or 1.
        for cap in 0..ITERATION_WARN_MIN_CAP {
            for current in 0..=cap.saturating_add(2) {
                assert!(
                    !should_warn_at_iteration_threshold(current, cap),
                    "should not warn at current={current} cap={cap}"
                );
            }
        }
    }

    #[test]
    fn iteration_warning_handles_minimum_warnable_cap_boundary() {
        // Cap == ITERATION_WARN_MIN_CAP (5): (5 * 4) / 5 = 4 → warn at 4+.
        assert!(!should_warn_at_iteration_threshold(3, 5));
        assert!(should_warn_at_iteration_threshold(4, 5));
        assert!(should_warn_at_iteration_threshold(5, 5));
    }

    #[test]
    fn iteration_warning_handles_overflow_resistant_caps() {
        // SDK callers that write `AgentConfig::max_tool_iterations = usize::MAX`
        // directly bypass the resolvers' clamp. Without `saturating_mul`,
        // `max * 4` would wrap to a tiny number and the warning would fire
        // on iteration ~0. The saturating multiply pins the threshold at
        // (saturated) usize::MAX / 5, so the warning effectively never
        // fires for absurd caps — which is the safer default.
        assert!(!should_warn_at_iteration_threshold(1_000_000, usize::MAX));
        assert!(!should_warn_at_iteration_threshold(
            usize::MAX / 6,
            usize::MAX
        ));
        // Conversely, a current at the saturated threshold should fire.
        assert!(should_warn_at_iteration_threshold(
            usize::MAX / 5,
            usize::MAX
        ));
    }

    #[test]
    fn iteration_handoff_steering_text_is_self_describing() {
        // Pinning the wording is intentional: this string is the load-bearing
        // contract between the runtime and the agent's iteration-aware-handoff
        // protocol. If it changes, downstream spec templates may need an
        // update, so the test forces a deliberate review on edits.
        let text = iteration_handoff_steering_text(42, 50);
        assert!(text.contains("[runtime]"));
        assert!(text.contains("Tool-iteration budget at >=80%"));
        assert!(text.contains("used 42 of 50"));
        assert!(text.contains("graceful handoff"));
        assert!(text.contains("incomplete-handoff"));
        assert!(text.contains("Do NOT compress"));
    }

    #[test]
    fn filter_image_blocks_replaces_images_with_deduped_placeholder_text() {
        let mut blocks = vec![
            sample_image_block(),
            sample_image_block(),
            ContentBlock::Text(TextContent::new("tail")),
            sample_image_block(),
        ];

        let removed = filter_image_blocks(&mut blocks, BLOCK_IMAGES_PLACEHOLDER);

        assert_eq!(removed, 3);
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(matches!(
            blocks.first(),
            Some(ContentBlock::Text(TextContent { text, .. }))
                if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)
        ));
        assert!(matches!(
            blocks.get(1),
            Some(ContentBlock::Text(TextContent { text, .. })) if text.as_str().eq("tail")
        ));
        assert!(matches!(
            blocks.get(2),
            Some(ContentBlock::Text(TextContent { text, .. }))
                if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)
        ));
    }

    #[test]
    fn filter_images_for_provider_filters_images_from_all_block_message_types() {
        let mut messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("hello")),
                    sample_image_block(),
                ]),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage {
                content: vec![sample_image_block()],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            })),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "tc1".to_string(),
                tool_name: "read".to_string(),
                content: vec![
                    sample_image_block(),
                    ContentBlock::Text(TextContent::new("ok")),
                ],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];

        let stats = filter_images_for_provider(&mut messages, BLOCK_IMAGES_PLACEHOLDER);

        assert_eq!(stats.removed_images, 3);
        assert_eq!(stats.affected_messages, 3);
        assert_eq!(
            messages.iter().map(image_count_in_message).sum::<usize>(),
            0,
            "no images should remain in provider-bound context"
        );
    }

    #[test]
    fn build_context_strips_images_when_block_images_enabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: true,
                model_accepts_images: true,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
                max_time: None,
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
                approval_state: None,
                bash_settings: None,
                secrets: None,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 0);
        assert!(matches!(
            &context.messages[0],
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(TextContent { text, .. }) if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)))
        ));
    }

    #[test]
    fn build_context_keeps_images_when_block_images_disabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
                model_accepts_images: true,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
                max_time: None,
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
                approval_state: None,
                bash_settings: None,
                secrets: None,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 1);
    }

    #[test]
    fn model_image_capability_filters_requests_without_losing_history() {
        let mut assistant = assistant_message("assistant text");
        assistant.content = vec![
            ContentBlock::Text(TextContent {
                text: "signed text".to_string(),
                text_signature: Some("preserved-signature".to_string()),
            }),
            sample_image_block(),
        ];
        let media = ContentBlock::Media(crate::model::MediaContent {
            data: "YXVkaW8=".to_string(),
            mime_type: "audio/wav".to_string(),
            name: Some("recording.wav".to_string()),
        });
        let original = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("before")),
                    sample_image_block(),
                    sample_image_block(),
                    ContentBlock::Text(TextContent::new("after")),
                ]),
                timestamp: 7,
            }),
            Message::assistant(assistant),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "read-image".to_string(),
                tool_name: "read".to_string(),
                content: vec![sample_image_block(), media.clone()],
                details: Some(json!({"type": "image", "data": "opaque tool metadata"})),
                is_error: false,
                timestamp: 11,
            }),
            Message::Custom(crate::model::CustomMessage {
                content: "custom text".to_string(),
                custom_type: "image-test".to_string(),
                display: true,
                details: Some(json!({"type": "image", "data": "opaque custom metadata"})),
                timestamp: 13,
            }),
        ];
        let original_json = serde_json::to_value(&original).unwrap();
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig::default(),
        );
        agent.replace_messages(original);
        assert_eq!(
            serde_json::to_value(&agent.build_context().messages).unwrap(),
            original_json,
        );

        agent.set_model_accepts_images(false);
        let context = agent.build_context();
        assert_eq!(context.messages.len(), 4);
        assert!(!context.messages.iter().any(message_has_images));
        let projected = serde_json::to_value(&context.messages).unwrap();
        assert_eq!(
            projected[0]["content"],
            json!([
                {"type": "text", "text": "before"},
                {"type": "text", "text": UNSUPPORTED_IMAGES_PLACEHOLDER},
                {"type": "text", "text": "after"}
            ]),
        );
        assert_eq!(projected[1]["content"][0], original_json[1]["content"][0]);
        assert_eq!(projected[1]["usage"], original_json[1]["usage"]);
        assert_eq!(projected[2]["toolCallId"], "read-image");
        assert_eq!(
            projected[2]["content"][1],
            serde_json::to_value(media).unwrap()
        );
        assert_eq!(projected[2]["details"], original_json[2]["details"]);
        assert_eq!(projected[3], original_json[3]);
        drop(context);
        assert_eq!(
            serde_json::to_value(agent.messages()).unwrap(),
            original_json
        );

        agent.set_model_accepts_images(true);
        assert_eq!(
            serde_json::to_value(&agent.build_context().messages).unwrap(),
            original_json,
            "returning to a vision model must restore the original image bytes",
        );
    }

    #[test]
    fn image_policy_keeps_image_free_history_shared() {
        let assistant = Arc::new(assistant_message("unchanged assistant"));
        let tool = Arc::new(ToolResultMessage {
            tool_call_id: "text-tool".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "unchanged tool result",
            ))],
            details: None,
            is_error: false,
            timestamp: 0,
        });
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig {
                model_accepts_images: false,
                ..AgentConfig::default()
            },
        );
        agent.replace_messages(vec![
            Message::Assistant(Arc::clone(&assistant)),
            Message::ToolResult(Arc::clone(&tool)),
        ]);
        assert!(matches!(agent.build_context().messages, Cow::Borrowed(_)));

        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));
        let context = agent.build_context();
        assert!(
            matches!(&context.messages[0], Message::Assistant(value) if Arc::ptr_eq(value, &assistant))
        );
        assert!(
            matches!(&context.messages[1], Message::ToolResult(value) if Arc::ptr_eq(value, &tool))
        );
        assert!(!context.messages.iter().any(message_has_images));
    }

    #[test]
    fn bare_provider_switch_does_not_inherit_image_capability() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig::default(),
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));
        assert!(
            agent
                .build_context()
                .messages
                .iter()
                .any(message_has_images)
        );
        agent.set_provider(Arc::new(SilentProvider));
        assert!(
            !agent
                .build_context()
                .messages
                .iter()
                .any(message_has_images)
        );
        assert!(agent.messages().iter().any(message_has_images));
        agent.set_model_accepts_images(true);
        assert!(
            agent
                .build_context()
                .messages
                .iter()
                .any(message_has_images)
        );
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)]
    fn context_hook_images_obey_the_final_provider_image_policy() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("context-image.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("context", async (event) => ({
                    messages: [...event.messages, {
                      role: "user",
                      content: [
                        { type: "text", text: "context hook reached provider" },
                        { type: "image", mimeType: "image/png", data: "aGVsbG8=" }
                      ],
                      timestamp: 17
                    }]
                  }));
                }
                "#,
            )
            .expect("write extension");

            for (block_images, model_accepts_images, expected_images, placeholder) in [
                (false, false, 0, Some(UNSUPPORTED_IMAGES_PLACEHOLDER)),
                (true, true, 0, Some(BLOCK_IMAGES_PLACEHOLDER)),
                (false, true, 1, None),
            ] {
                let provider = CapturingProvider::new("test-api");
                let calls = provider.calls();
                let agent = Agent::new(
                    Arc::new(provider),
                    ToolRegistry::from_tools(Vec::new()),
                    AgentConfig {
                        model_accepts_images,
                        block_images,
                        ..AgentConfig::default()
                    },
                );
                let mut session = AgentSession::new(
                    agent,
                    Arc::new(Mutex::new(Session::in_memory())),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                session
                    .enable_extensions(
                        &[],
                        temp_dir.path(),
                        None,
                        std::slice::from_ref(&entry_path),
                    )
                    .await
                    .expect("enable context extension");
                session
                    .run_text("hello".to_string(), |_| {})
                    .await
                    .expect("complete provider turn");

                let recorded = calls.lock().expect("captured requests");
                assert_eq!(recorded.len(), 1);
                let messages = &recorded[0].messages;
                assert_eq!(
                    messages.iter().map(image_count_in_message).sum::<usize>(),
                    expected_images,
                );
                let projected = serde_json::to_value(messages).unwrap();
                let injected = projected.as_array().unwrap().last().unwrap();
                assert_eq!(
                    injected["content"][0]["text"], "context hook reached provider",
                    "the extension must actually rewrite the provider context",
                );
                if let Some(placeholder) = placeholder {
                    assert_eq!(injected["content"][1]["text"], placeholder);
                }
            }
        });
    }

    #[test]
    fn auto_compaction_start_serializes_with_pi_mono_compatible_type_tag() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_with_pi_mono_compatible_fields() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({"tokens_before": 5000, "tokens_after": 2000})),
            aborted: false,
            will_retry: false,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], false);
        assert!(json.get("errorMessage").is_none()); // skipped when None
        assert!(json["result"].is_object());
    }

    #[test]
    fn auto_compaction_end_includes_error_message_when_present() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: true,
            will_retry: false,
            error_message: Some("Compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], true);
        assert_eq!(json["errorMessage"], "Compaction failed");
    }

    #[test]
    fn auto_retry_start_serializes_with_camel_case_fields() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "Rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);
        assert_eq!(json["errorMessage"], "Rate limited");
    }

    #[test]
    fn auto_retry_end_serializes_success_and_omits_null_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_includes_final_error_on_failure() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("Max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "Max retries exceeded");
    }

    #[test]
    fn message_queue_push_increments_seq_and_counts_both_queues() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        assert_eq!(queue.pending_count(), 0);

        assert_eq!(queue.push_steering(queued_user_message("s1")), 0);
        assert_eq!(queue.push_follow_up(queued_user_message("f1")), 1);
        assert_eq!(queue.push_steering(queued_user_message("s2")), 2);

        assert_eq!(queue.pending_count(), 3);
    }

    #[test]
    fn message_queue_pop_steering_one_at_a_time_preserves_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(first[0].message(), "s1");
        assert_eq!(queue.pending_count(), 1);

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert_user_text(second[0].message(), "s2");
        assert_eq!(queue.pending_count(), 0);

        let empty = queue.pop_steering();
        assert!(empty.is_empty());
    }

    #[test]
    fn message_queue_pop_respects_queue_modes_per_kind() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));
        queue.push_follow_up(queued_user_message("f1"));
        queue.push_follow_up(queued_user_message("f2"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 2);
        assert_user_text(steering[0].message(), "s1");
        assert_user_text(steering[1].message(), "s2");
        assert_eq!(queue.pending_count(), 2);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(follow_up[0].message(), "f1");
        assert_eq!(queue.pending_count(), 1);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(follow_up[0].message(), "f2");
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_set_modes_applies_to_existing_messages() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(first[0].message(), "s1");

        queue.set_modes(QueueMode::All, QueueMode::OneAtATime);
        let remaining = queue.pop_steering();
        assert_eq!(remaining.len(), 1);
        assert_user_text(remaining[0].message(), "s2");
    }

    fn build_switch_test_session(auth: &AuthStorage) -> AgentSession {
        let registry = ModelRegistry::load(auth, None);
        let current_entry = registry
            .find("anthropic", "claude-sonnet-4-5")
            .expect("anthropic model in registry");
        let provider = crate::providers::create_provider(&current_entry, None)
            .expect("create anthropic provider");
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut stream_options = StreamOptions {
            api_key: Some("stale-key".to_string()),
            ..Default::default()
        };
        let _ = stream_options
            .headers
            .insert("x-stale-header".to_string(), "stale-value".to_string());
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options,
                block_images: false,
                model_accepts_images: true,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
                max_time: None,
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
                approval_state: None,
                bash_settings: None,
                secrets: None,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-sonnet-4-5".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);
        agent_session.set_auth_storage(auth.clone());
        agent_session
    }

    fn extension_switch_entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: format!("https://{provider}.example.test/v1"),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some(format!("{provider}-key")),
            headers: HashMap::from([("x-route".to_string(), provider.to_string())]),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn build_catalog_switch_session(
        entries: Vec<ModelEntry>,
        initial: (&str, &str),
        cwd: &Path,
    ) -> AgentSession {
        let registry = ModelRegistry::from_entries_for_tests(entries);
        let initial_entry = registry.find(initial.0, initial.1).expect("initial entry");
        let provider =
            crate::providers::create_provider(&initial_entry, None).expect("initial provider");
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], cwd, None),
            AgentConfig {
                stream_options: StreamOptions {
                    api_key: initial_entry.api_key.clone(),
                    headers: initial_entry.headers.clone(),
                    thinking_level: Some(crate::model::ThinkingLevel::Off),
                    ..StreamOptions::default()
                },
                ..AgentConfig::default()
            },
        );
        let mut session = Session::in_memory();
        session.append_model_change(initial.0.to_string(), initial.1.to_string());
        session.append_thinking_level_change("off".to_string());
        session.set_model_header(
            Some(initial.0.to_string()),
            Some(initial.1.to_string()),
            Some("off".to_string()),
        );
        AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        )
        .with_model_registry(registry)
    }

    async fn build_extension_switch_session(
        entries: Vec<ModelEntry>,
        initial: (&str, &str),
        cwd: &Path,
        extension_source: &str,
    ) -> AgentSession {
        let mut agent_session = build_catalog_switch_session(entries, initial, cwd);
        let extension_path = cwd.join("model-switch.mjs");
        std::fs::write(&extension_path, extension_source).expect("write model switch extension");
        agent_session
            .enable_extensions(&[], cwd, None, &[extension_path])
            .await
            .expect("enable model switch extension");
        agent_session
    }

    async fn try_catalog_failover(
        agent_session: &mut AgentSession,
        available_models: &[ModelEntry],
        auth: &AuthStorage,
        specs: &[&str],
        start_position: usize,
        cli_api_key: Option<&str>,
    ) -> FailoverSwapOutcome {
        let chain = crate::failover::FailoverChain {
            entries: specs.iter().map(|spec| (*spec).to_string()).collect(),
        };
        let attempt = FailoverSwapAttempt {
            chain: &chain,
            start_position,
            available_models,
            auth,
            cli_api_key,
            class: crate::failover::FailoverClass::Quota,
            thinking_level_to_clamp: crate::model::ThinkingLevel::Off,
            require_incomplete_tail: false,
            primary: None,
            cooldown_secs: Some(0),
            lifecycle_id: Some("catalog-routing-test"),
        };
        agent_session
            .try_failover(&crate::agent_cx::AgentCx::for_request(), &attempt)
            .await
            .expect("failover decision")
    }

    #[test]
    fn extension_model_switch_preserves_declared_provider_through_runtime_and_reopen() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for (previous, requested) in
                [("openrouter", "open-router"), ("open-router", "openrouter")]
            {
                for reversed in [false, true] {
                    for requested_model in ["shared", "vendor/other-model"] {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let mut entries = vec![
                            extension_switch_entry(previous, "shared"),
                            extension_switch_entry(requested, "shared"),
                            extension_switch_entry(requested, "vendor/other-model"),
                        ];
                        if reversed {
                            entries.reverse();
                        }
                        let mut agent_session = build_extension_switch_session(
                            entries,
                            (previous, "shared"),
                            dir.path(),
                            r#"export default function init(pi) {
                                pi.registerCommand("select-route", {
                                    handler: async (args) => {
                                        await pi.events("setModel", JSON.parse(args));
                                        return "selected";
                                    }
                                });
                            }"#,
                        )
                        .await;
                        let request = json!({
                            "provider": requested,
                            "modelId": requested_model,
                        })
                        .to_string();
                        for _ in 0..2 {
                            agent_session
                                .execute_extension_command(
                                    "select-route",
                                    &request,
                                    5_000,
                                    |_| {},
                                )
                                .await
                                .expect("select registered destination through JS hostcall");
                            agent_session
                                .sync_runtime_selection_from_session_header()
                                .await
                                .expect("install selected runtime route");
                        }
                        let active = agent_session.agent.provider();
                        assert_eq!(active.name(), requested);
                        assert_eq!(active.model_id(), requested_model);
                        let entry = agent_session.current_model_entry().expect("active entry");
                        assert_eq!(
                            entry.model.base_url,
                            format!("https://{requested}.example.test/v1")
                        );
                        assert_eq!(agent_session.agent.stream_options().api_key, entry.api_key);
                        assert_eq!(agent_session.agent.stream_options().headers, entry.headers);

                        let cx = crate::agent_cx::AgentCx::for_request();
                        let mut session = agent_session
                            .session
                            .lock(cx.cx())
                            .await
                            .expect("session");
                        let model_changes = session
                            .entries
                            .iter()
                            .filter(|entry| {
                                matches!(entry, crate::session::SessionEntry::ModelChange(_))
                            })
                            .count();
                        assert_eq!(model_changes, 2, "repeat selection must be idempotent");
                        let path = dir.path().join("selected.jsonl");
                        session.path = Some(path.clone());
                        session.save().await.expect("save selected route");
                        drop(session);
                        let reopened = Session::open(path.to_str().expect("session path"))
                            .await
                            .expect("reopen selected route");
                        assert_eq!(reopened.header.provider.as_deref(), Some(requested));
                        assert_eq!(
                            reopened.effective_model_for_current_path(),
                            Some((requested.to_string(), requested_model.to_string()))
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn extension_model_switch_resolves_native_alias_and_refreshed_catalog() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let canonical = extension_switch_entry("openrouter", "shared");
            let custom = extension_switch_entry("open-router", "other");
            let mut agent_session = build_extension_switch_session(
                vec![canonical.clone()],
                ("openrouter", "shared"),
                dir.path(),
                "export default function init() {}",
            )
            .await;
            let bridge = agent_session
                .extensions
                .as_ref()
                .expect("extensions")
                .manager()
                .session_handle()
                .expect("session bridge");
            let origin = agent_session.session_action_admission.capture_origin();
            bridge
                .set_model(
                    "open-router".to_string(),
                    "SHARED".to_string(),
                    Some(origin.clone()),
                )
                .await
                .expect("undeclared native alias resolves");
            let before = bridge.get_branch().await;
            assert_eq!(
                bridge.get_model().await,
                (Some("openrouter".to_string()), Some("shared".to_string()))
            );
            assert_eq!(
                before
                    .iter()
                    .filter(|entry| entry["type"] == "model_change")
                    .count(),
                1
            );

            // A bridge created before registration must see the newly declared
            // provider boundary, including a missing model on that provider.
            agent_session.set_model_registry(ModelRegistry::from_entries_for_tests(vec![
                canonical, custom,
            ]));
            let error = bridge
                .set_model(
                    "open-router".to_string(),
                    "shared".to_string(),
                    Some(origin.clone()),
                )
                .await
                .expect_err("declared custom provider cannot borrow the native model");
            assert!(error.to_string().contains("open-router/shared"));
            assert!(error.to_string().contains("ctx.modelRegistry.find"));
            assert_eq!(bridge.get_branch().await, before);
            for (provider, model) in
                [("missing", "shared"), (" ", "shared"), ("openrouter", "\t")]
            {
                bridge
                    .set_model(
                        provider.to_string(),
                        model.to_string(),
                        Some(origin.clone()),
                    )
                    .await
                    .expect_err("unresolvable selection must not mutate session");
                assert_eq!(bridge.get_branch().await, before);
            }
            bridge
                .set_model("open-router".to_string(), "other".to_string(), Some(origin))
                .await
                .expect("newly registered model resolves through existing bridge");
            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("install custom destination");
            assert_eq!(agent_session.agent.provider().name(), "open-router");
            assert_eq!(agent_session.agent.provider().model_id(), "other");
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("open-router-key")
            );
        });
    }

    #[test]
    fn extension_startup_hook_uses_the_registered_model_catalog() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut agent_session = build_extension_switch_session(
                vec![
                    extension_switch_entry("openrouter", "shared"),
                    extension_switch_entry("open-router", "shared"),
                ],
                ("openrouter", "shared"),
                dir.path(),
                r#"export default function init(pi) {
                    pi.on("session_start", async () => {
                        await pi.events("setModel", { provider: "open-router", modelId: "shared" });
                    });
                }"#,
            )
            .await;
            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("install startup hook selection");
            assert_eq!(agent_session.agent.provider().name(), "open-router");
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("open-router-key")
            );
        });
    }

    #[test]
    fn extension_startup_hook_can_select_its_newly_registered_provider() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut agent_session = build_extension_switch_session(
                vec![extension_switch_entry("openrouter", "shared")],
                ("openrouter", "shared"),
                dir.path(),
                r#"export default function init(pi) {
                    pi.registerProvider("open-router", {
                        api: "openai-completions",
                        baseUrl: "https://newly-registered.example.test/v1",
                        apiKey: "newly-registered-key",
                        models: [{ id: "shared", name: "Shared", contextWindow: 64000, maxTokens: 4096 }]
                    });
                    pi.on("session_start", async (_event, ctx) => {
                        if (!ctx.modelRegistry.find("open-router", "shared")) {
                            throw new Error("registered provider missing from startup catalog");
                        }
                        await pi.events("setModel", { provider: "open-router", modelId: "shared" });
                    });
                }"#,
            )
            .await;
            let manager = agent_session
                .extensions
                .as_ref()
                .expect("extension region")
                .manager();
            let bridge = manager.session_handle().expect("session bridge");
            assert_eq!(
                bridge.get_model().await,
                (Some("open-router".to_string()), Some("shared".to_string()))
            );

            // The caller's final registration merge remains authoritative for
            // live routing, just as main and SDK startup complete this step.
            let bindings =
                crate::models::extension_provider_bindings(&manager.extension_providers())
                    .expect("registered provider bindings");
            let mut registry = agent_session.model_registry().expect("registry").clone();
            registry
                .merge_extension_registry(&bindings, manager.extension_model_entries())
                .expect("final registration merge");
            agent_session.set_model_registry(registry);
            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("install newly registered destination");
            assert_eq!(agent_session.agent.provider().name(), "open-router");
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("newly-registered-key")
            );
            assert_eq!(
                agent_session
                    .current_model_entry()
                    .expect("active entry")
                    .model
                    .base_url,
                "https://newly-registered.example.test/v1"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one fixture run in both catalog orders
    fn failover_keeps_full_catalog_identity_and_exact_admission() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for reversed in [false, true] {
                for (ready, admitted, override_key, should_select_declared) in [
                    (false, false, None, false),
                    (false, false, Some(" "), false),
                    (false, false, Some(" explicit-key "), true),
                    (false, true, None, true),
                    (true, false, None, false),
                    (true, false, Some("explicit-key"), false),
                    (true, true, None, true),
                ] {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let native = extension_switch_entry("openrouter", "shared");
                    let initial = extension_switch_entry("initial-route", "shared");
                    let rescue = extension_switch_entry("rescue-route", "shared");
                    let mut declared = extension_switch_entry("open-router", "shared");
                    if !ready {
                        declared.api_key = None;
                    }
                    let mut catalog = vec![
                        native.clone(),
                        declared.clone(),
                        initial.clone(),
                        rescue.clone(),
                    ];
                    if reversed {
                        catalog.reverse();
                    }
                    let mut available = vec![native, initial, rescue];
                    if admitted {
                        available.push(declared);
                    }
                    let mut agent_session = build_catalog_switch_session(
                        catalog,
                        ("initial-route", "shared"),
                        dir.path(),
                    );
                    let mut auth =
                        AuthStorage::load(dir.path().join("auth.json")).expect("auth");
                    for provider in ["openrouter", "open-router", "rescue-route"] {
                        auth.set(
                            provider,
                            AuthCredential::BearerToken {
                                token: format!("{provider}-token"),
                            },
                        );
                    }
                    let outcome = try_catalog_failover(
                        &mut agent_session,
                        &available,
                        &auth,
                        &["open-router/shared", "rescue-route/shared"],
                        0,
                        override_key,
                    )
                    .await;
                    let committed = outcome.committed.expect("usable fallback");
                    let expected = if should_select_declared {
                        "open-router"
                    } else {
                        "rescue-route"
                    };
                    let expected_index = usize::from(!should_select_declared);
                    assert_eq!(committed.entry_index, expected_index);
                    assert_eq!(outcome.next_position, expected_index + 1);
                    assert_eq!(committed.to_provider, expected);
                    assert_eq!(agent_session.agent.provider().name(), expected);
                    assert_eq!(agent_session.agent.provider().model_id(), "shared");
                    let expected_key = override_key
                        .map(str::trim)
                        .filter(|key| !key.is_empty())
                        .map_or_else(|| format!("{expected}-token"), str::to_string);
                    assert_eq!(
                        agent_session.agent.stream_options().api_key.as_deref(),
                        Some(expected_key.as_str())
                    );
                    assert_eq!(
                        agent_session.agent.stream_options().headers.get("x-route"),
                        Some(&expected.to_string())
                    );
                    let entry = agent_session.current_model_entry().expect("selected entry");
                    assert_eq!(
                        entry.model.base_url,
                        format!("https://{expected}.example.test/v1")
                    );
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let session = agent_session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("session");
                    assert_eq!(session.header.provider.as_deref(), Some(expected));
                    assert_eq!(
                        session.effective_model_for_current_path(),
                        Some((expected.to_string(), "shared".to_string()))
                    );
                    let provenance = session
                        .active_failover_provenance_for_current_path()
                        .expect("committed failover provenance");
                    assert_eq!(provenance.primary_provider, "initial-route");
                    assert_eq!(provenance.fallback_provider, expected);
                    assert_eq!(provenance.chain_position, Some(expected_index + 1));
                }
            }
        });
    }

    #[test]
    fn failover_walk_keeps_distinct_declared_aliases_and_resumed_entries() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for (previous, requested) in [
                ("openrouter", "open-router"),
                ("open-router", "openrouter"),
            ] {
                for reversed in [false, true] {
                    for resumed in [false, true] {
                        let dir = tempfile::tempdir().expect("tempdir");
                        let mut entries = vec![
                            extension_switch_entry(previous, "shared"),
                            extension_switch_entry(requested, "shared"),
                            extension_switch_entry("intermediate-route", "shared"),
                        ];
                        if reversed {
                            entries.reverse();
                        }
                        let initial = if resumed {
                            "intermediate-route"
                        } else {
                            previous
                        };
                        let mut agent_session = build_catalog_switch_session(
                            entries.clone(),
                            (initial, "shared"),
                            dir.path(),
                        );
                        let mut auth =
                            AuthStorage::load(dir.path().join("auth.json")).expect("auth");
                        auth.set(
                            requested,
                            AuthCredential::BearerToken {
                                token: format!("{requested}-token"),
                            },
                        );
                        let previous_spec = format!("{previous}/shared");
                        let requested_spec = format!("{requested}/shared");
                        let mut specs = vec![previous_spec.as_str()];
                        if resumed {
                            specs.push("intermediate-route/shared");
                        }
                        specs.push(&requested_spec);
                        let expected_index = specs.len() - 1;
                        let outcome = try_catalog_failover(
                            &mut agent_session,
                            &entries,
                            &auth,
                            &specs,
                            if resumed { expected_index } else { 0 },
                            None,
                        )
                        .await;
                        let committed = outcome.committed.expect("distinct provider fallback");
                        assert_eq!(committed.entry_index, expected_index);
                        assert_eq!(outcome.next_position, specs.len());
                        assert_eq!(committed.to_provider, requested);
                        assert_eq!(agent_session.agent.provider().name(), requested);
                        assert_eq!(agent_session.agent.provider().model_id(), "shared");
                        assert_eq!(
                            agent_session.agent.stream_options().api_key.as_deref(),
                            Some(format!("{requested}-token").as_str())
                        );
                        assert_eq!(
                            agent_session.agent.stream_options().headers.get("x-route"),
                            Some(&requested.to_string())
                        );
                        let cx = crate::agent_cx::AgentCx::for_request();
                        let session = agent_session
                            .session
                            .lock(cx.cx())
                            .await
                            .expect("session");
                        assert_eq!(session.header.provider.as_deref(), Some(requested));
                        assert_eq!(
                            session
                                .active_failover_provenance_for_current_path()
                                .expect("failover provenance")
                                .fallback_provider,
                            requested
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn failover_preserves_native_alias_dedup_and_unlisted_destinations() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for (spec, provider, override_key) in [
                ("ollama/vendor/unlisted", "ollama", None),
                (
                    "openrouter/vendor/unlisted",
                    "openrouter",
                    Some("unlisted-key"),
                ),
            ] {
                let dir = tempfile::tempdir().expect("tempdir");
                let entries = vec![extension_switch_entry("openrouter", "shared")];
                let mut agent_session = build_catalog_switch_session(
                    entries.clone(),
                    ("openrouter", "shared"),
                    dir.path(),
                );
                let auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
                let no_op = try_catalog_failover(
                    &mut agent_session,
                    &entries,
                    &auth,
                    &["open-router/shared", " OPENROUTER / SHARED "],
                    0,
                    None,
                )
                .await;
                assert!(no_op.committed.is_none());
                assert_eq!(no_op.next_position, 2);
                assert_eq!(agent_session.agent.provider().name(), "openrouter");
                {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let session = agent_session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("session");
                    assert!(session.active_failover_provenance_for_current_path().is_none());
                    assert_eq!(session.entries.len(), 2);
                }
                let outcome = try_catalog_failover(
                    &mut agent_session,
                    &entries,
                    &auth,
                    &["open-router/shared", "openrouter/shared", spec],
                    0,
                    override_key,
                )
                .await;
                let committed = outcome.committed.expect("unlisted native fallback");
                assert_eq!(committed.entry_index, 2);
                assert_eq!(outcome.next_position, 3);
                assert_eq!(agent_session.agent.provider().name(), provider);
                assert_eq!(agent_session.agent.provider().model_id(), "vendor/unlisted");
                if let Some(key) = override_key {
                    assert_eq!(
                        agent_session.agent.stream_options().api_key.as_deref(),
                        Some(key)
                    );
                }
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one fixture run in both catalog orders
    fn primary_restore_keeps_full_catalog_identity_and_respects_admission() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for reversed in [false, true] {
                for (ready, admitted, override_key, should_restore) in [
                    (true, true, None, true),
                    (false, false, None, false),
                    (false, false, Some(" "), false),
                    (false, false, Some("explicit-key"), true),
                    (true, false, None, false),
                    (true, false, Some("explicit-key"), false),
                ] {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let native = extension_switch_entry("openrouter", "shared");
                    let fallback = extension_switch_entry("fallback-route", "shared");
                    let mut primary_entry = extension_switch_entry("open-router", "shared");
                    primary_entry.model.base_url =
                        "https://declared-primary.example.test/v1".to_string();
                    if !ready {
                        primary_entry.api_key = None;
                    }
                    let mut full_catalog =
                        vec![native.clone(), primary_entry.clone(), fallback.clone()];
                    if reversed {
                        full_catalog.reverse();
                    }
                    let mut allowed = vec![native, fallback];
                    if admitted {
                        allowed.push(primary_entry);
                    }
                    let mut agent_session = build_catalog_switch_session(
                        full_catalog,
                        ("fallback-route", "shared"),
                        dir.path(),
                    );
                    let mut auth =
                        AuthStorage::load(dir.path().join("auth.json")).expect("auth");
                    auth.set(
                        "open-router",
                        AuthCredential::BearerToken {
                            token: "declared-provider-token".to_string(),
                        },
                    );
                    auth.set(
                        "openrouter",
                        AuthCredential::BearerToken {
                            token: "competing-provider-token".to_string(),
                        },
                    );
                    let primary = crate::failover::FailoverPrimary {
                        provider: "open-router".to_string(),
                        model_id: "shared".to_string(),
                        requested_thinking_level: crate::model::ThinkingLevel::Off,
                    };
                    let active = ("fallback-route".to_string(), "shared".to_string());
                    let request = PrimaryRestoreRequest {
                        primary: &primary,
                        active: &active,
                        cooldown_elapsed: true,
                        available_models: &allowed,
                        auth: &auth,
                        cli_api_key: override_key,
                        strict_invariants: false,
                        invalidate_background_compaction: true,
                    };
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let restored = agent_session
                        .restore_primary(&cx, &request)
                        .await
                        .expect("restore decision");
                    assert_eq!(restored.is_some(), should_restore);
                    if should_restore {
                        assert_eq!(agent_session.agent.provider().name(), "open-router");
                        assert_eq!(agent_session.agent.provider().model_id(), "shared");
                        assert_eq!(
                            agent_session.agent.stream_options().api_key.as_deref(),
                            Some(override_key.unwrap_or("declared-provider-token"))
                        );
                        assert_eq!(
                            agent_session
                                .current_model_entry()
                                .expect("restored entry")
                                .model
                                .base_url,
                            "https://declared-primary.example.test/v1"
                        );
                    } else {
                        assert_eq!(agent_session.agent.provider().name(), "fallback-route");
                    }
                    let session = agent_session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("session");
                    let expected_provider = if should_restore {
                        "open-router"
                    } else {
                        "fallback-route"
                    };
                    assert_eq!(session.header.provider.as_deref(), Some(expected_provider));
                    assert_eq!(
                        session.effective_model_for_current_path(),
                        Some((expected_provider.to_string(), "shared".to_string()))
                    );
                }
            }
        });
    }

    #[test]
    fn primary_restore_rejects_stale_declared_alias_fallback_identity() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for runtime_provider in ["open-router", "openrouter"] {
                let dir = tempfile::tempdir().expect("tempdir");
                let entries = vec![
                    extension_switch_entry("openrouter", "shared"),
                    extension_switch_entry("open-router", "shared"),
                    extension_switch_entry("primary-route", "shared"),
                ];
                let mut agent_session = build_catalog_switch_session(
                    entries.clone(),
                    (runtime_provider, "shared"),
                    dir.path(),
                );
                let cx = crate::agent_cx::AgentCx::for_request();
                // One case has drifted runtime identity, the other has a
                // branch-local Session change despite the runtime still matching.
                {
                    let mut session = agent_session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("session");
                    session.select_model("open-router".to_string(), "shared".to_string());
                }
                let auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
                let primary = crate::failover::FailoverPrimary {
                    provider: "primary-route".to_string(),
                    model_id: "shared".to_string(),
                    requested_thinking_level: crate::model::ThinkingLevel::Off,
                };
                let active = ("openrouter".to_string(), "shared".to_string());
                let request = PrimaryRestoreRequest {
                    primary: &primary,
                    active: &active,
                    cooldown_elapsed: true,
                    available_models: &entries,
                    auth: &auth,
                    cli_api_key: None,
                    strict_invariants: false,
                    invalidate_background_compaction: true,
                };
                assert!(
                    agent_session
                        .restore_primary(&cx, &request)
                        .await
                        .expect("decline stale restore")
                        .is_none()
                );
                assert_eq!(agent_session.agent.provider().name(), runtime_provider);
                let session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session");
                assert_eq!(session.header.provider.as_deref(), Some("open-router"));
            }
        });
    }

    #[test]
    fn compaction_runtime_handle_creates_fallback_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");
        let mut agent_session = build_switch_test_session(&auth);

        assert!(agent_session.compaction_runtime.is_none());
        assert!(agent_session.runtime_handle.is_none());

        let runtime_handle = agent_session
            .compaction_runtime_handle()
            .expect("create fallback compaction runtime");
        let join = runtime_handle.spawn(async { 7_u8 });
        assert_eq!(futures::executor::block_on(join), 7);

        assert!(agent_session.compaction_runtime.is_some());
        assert!(agent_session.runtime_handle.is_some());
    }

    #[test]
    fn provider_transition_waits_for_active_provider_permit() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let gate = ProviderAdmissionGate::default();
            let active_cx = crate::agent_cx::AgentCx::for_request();
            let active_provider = gate
                .acquire(active_cx.cx())
                .await
                .expect("acquire active provider permit");

            let transition_gate = gate.clone();
            let transition = runtime_handle.spawn(async move {
                let transition_cx = crate::agent_cx::AgentCx::for_request();
                let permit = transition_gate
                    .begin_transition(
                        "test transition interrupted".to_string(),
                        transition_cx.cx(),
                    )
                    .await
                    .expect("begin transition");
                drop(permit);
            });

            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(10),
            )
            .await;
            assert!(
                !transition.is_finished(),
                "transition must not pass an active provider future"
            );

            drop(active_provider);
            transition.await;
            assert_eq!(
                gate.reason().as_deref(),
                Some("test transition interrupted")
            );
            gate.clear();
            assert!(gate.reason().is_none());
        });
    }

    #[test]
    fn prepared_model_selection_updates_stream_credentials_and_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        let prepared = agent_session
            .prepare_model_selection("openai", "gpt-4o", crate::model::ThinkingLevel::Off)
            .expect("prepare switch");
        agent_session.install_prepared_model_selection(prepared);

        assert_eq!(agent_session.agent.provider().name(), "openai");
        assert_eq!(agent_session.agent.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("openai-key")
        );
        assert!(
            agent_session.agent.stream_options().headers.is_empty(),
            "stream headers should be refreshed from selected model entry"
        );
    }

    #[test]
    fn prepared_model_selection_clears_stale_key_for_keyless_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "local-model".to_string(),
                name: "Local Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme-local".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        let prepared = agent_session
            .prepare_model_selection(
                "acme-local",
                "local-model",
                crate::model::ThinkingLevel::Off,
            )
            .expect("prepare keyless local model");
        agent_session.install_prepared_model_selection(prepared);

        assert_eq!(agent_session.agent.provider().name(), "acme-local");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "stale key must be cleared when target model has no configured key"
        );

        agent_session.agent.stream_options_mut().api_key = Some("stale-again".to_string());
        let prepared = agent_session
            .prepare_model_selection(
                "acme-local",
                "local-model",
                crate::model::ThinkingLevel::Off,
            )
            .expect("prepare already-active keyless model");
        agent_session.install_prepared_model_selection(prepared);
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "an already-active keyless model must not retain a stale credential"
        );
    }

    #[test]
    fn prepared_model_selection_treats_blank_model_key_as_missing_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "blank-model".to_string(),
                name: "Blank Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("   ".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        let err = agent_session
            .prepare_model_selection("acme", "blank-model", crate::model::ThinkingLevel::Off)
            .err()
            .expect("blank keys must not satisfy credential requirements");

        assert!(
            err.to_string()
                .contains("Missing credentials for acme/blank-model"),
            "unexpected error: {err}"
        );
        assert_eq!(agent_session.agent.provider().name(), "anthropic");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            Some("stale-key".to_string()),
            "failed switches must preserve the prior runtime credentials"
        );
    }

    #[test]
    fn set_provider_model_preserves_session_header_when_switch_fails() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("missing-provider", "missing-model")
                .await
                .expect_err("missing model should not switch");
            assert!(
                err.to_string()
                    .contains("Unable to switch provider/model to missing-provider/missing-model"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_rejects_missing_credentials_without_switching() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            let model_scope = agent_session.agent.shared_tools().snapshot().model_scope();
            let original_model = model_scope.current();

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect_err("missing credentials should abort model switch");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(model_scope.current(), original_model);
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_quarantines_failed_persistence_without_runtime_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let handle = runtime.handle();

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "anthropic-key".to_string(),
                },
            );
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );
            let mut agent_session = build_switch_test_session(&auth);
            let model_scope = agent_session.agent.shared_tools().snapshot().model_scope();
            let original_model = model_scope.current();
            let blocked_path = dir.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.path = Some(blocked_path);
            }
            agent_session.save_enabled = true;
            let original_compaction_window =
                agent_session.compaction_settings().context_window_tokens;
            agent_session.park_pending_compaction_for_test(&handle, None);

            let err = agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect_err("unwritable model-selection candidate must fail closed");
            assert!(err.is_session_persistence(), "unexpected error: {err}");
            assert_eq!(
                model_scope.current(),
                original_model,
                "a failed model commit must not retarget delegated work"
            );
            assert!(
                agent_session.has_pending_background_compaction(),
                "a switch that never became durable must not retire the live model's compaction"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );
            assert_eq!(
                agent_session.agent.stream_options().api_key.as_deref(),
                Some("stale-key")
            );
            assert_eq!(
                agent_session.compaction_settings().context_window_tokens,
                original_compaction_window
            );
            assert!(agent_session.provider_admission.reason().is_some());

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
            assert!(
                session
                    .entries_for_current_path()
                    .iter()
                    .all(|entry| !matches!(entry, crate::session::SessionEntry::ModelChange(_)))
            );
            drop(session);

            let blocked = agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect_err("quarantine must block later provider re-entry");
            assert!(blocked.is_session_persistence());
        });
    }

    #[test]
    fn set_thinking_level_quarantines_failed_persistence_without_live_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            let blocked_path = dir.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.path = Some(blocked_path);
            }
            agent_session.save_enabled = true;
            let original_thinking = agent_session.agent.stream_options().thinking_level;

            let err = agent_session
                .set_thinking_level(crate::model::ThinkingLevel::High)
                .await
                .expect_err("unwritable thinking candidate must fail closed");
            assert!(err.is_session_persistence(), "unexpected error: {err}");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                original_thinking
            );
            assert!(agent_session.provider_admission.reason().is_some());

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert!(session.header.thinking_level.is_none());
            assert!(
                session
                    .entries_for_current_path()
                    .iter()
                    .all(|entry| !matches!(
                        entry,
                        crate::session::SessionEntry::ThinkingLevelChange(_)
                    ))
            );
        });
    }

    /// bd-5jfkl: a setter cancelled while its save is in flight. The write may
    /// still land on the blocking thread, so the durable outcome is unknown;
    /// the live runtime and live Session must stay as they were and provider
    /// re-entry must be quarantined rather than silently resumed.
    #[test]
    fn set_thinking_level_cancelled_mid_save_quarantines_without_live_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 1)
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth = AuthStorage::load(dir.path().join("auth.json")).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.session_dir = Some(dir.path().to_path_buf());
                session.path = Some(dir.path().join("cancelled.jsonl"));
            }
            agent_session.save_enabled = true;
            let original_thinking = agent_session.agent.stream_options().thinking_level;

            {
                let setter = agent_session.set_thinking_level(crate::model::ThinkingLevel::High);
                futures::pin_mut!(setter);
                assert!(
                    futures::poll!(setter.as_mut()).is_pending(),
                    "the first poll must park on the blocking save"
                );
            }

            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                original_thinking
            );
            assert!(
                agent_session.ensure_provider_reentry_allowed().is_err(),
                "a cancelled transition must leave provider re-entry quarantined"
            );
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock after cancellation");
            assert!(session.header.thinking_level.is_none());
        });
    }

    #[test]
    fn set_provider_model_clamps_thinking_for_non_reasoning_targets() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .set_provider_model("acme", "plain-model")
                .await
                .expect("switch should clamp unsupported thinking");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );
            assert_eq!(
                agent_session.agent.keyword_max_thinking_level,
                crate::model::ThinkingLevel::Off,
                "ultrathink must use the target model's clamped maximum"
            );
            assert!(
                !agent_session.agent.model_accepts_images(),
                "runtime model switches must install the target image policy"
            );
            assert_eq!(
                agent_session.compaction_settings().context_window_tokens,
                128_000,
                "runtime model switches must install the target compaction window"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("acme"));
            assert_eq!(session.header.model_id.as_deref(), Some("plain-model"));
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
        });
    }

    #[test]
    fn bare_provider_replacement_resets_catalog_controls_fail_closed() {
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
        agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
        agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);

        agent.set_provider(Arc::new(SilentProvider));

        assert_eq!(
            agent.keyword_max_thinking_level,
            crate::model::ThinkingLevel::Off,
            "provider replacement without registry metadata must not inherit the prior cap"
        );
        assert_eq!(
            agent.tool_call_dialect,
            crate::dialects::Dialect::Native,
            "provider replacement without registry metadata must not inherit repair opt-in"
        );
    }

    #[test]
    fn tool_model_scope_tracks_provider_swaps_without_changing_captured_selections() {
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let standalone_scope = tools.model_scope();
        assert!(standalone_scope.current().is_none());
        let shared = crate::tools::SharedToolRegistry::new(tools);
        let old_snapshot = shared.snapshot();
        let mut agent = Agent::with_shared_tools(
            Arc::new(SilentProvider),
            shared.clone(),
            AgentConfig {
                stream_options: StreamOptions {
                    api_key: Some("private-parent-credential".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let captured = standalone_scope.current().expect("initial model affinity");
        assert_eq!(captured.provider, "silent-provider");
        assert_eq!(captured.model, "test-model");

        let other = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig::default(),
        );
        let fallback = crate::models::ad_hoc_model_entry("openrouter", "vendor/fallback-model")
            .expect("fallback entry");
        agent.set_provider(
            crate::providers::create_provider(&fallback, None).expect("fallback provider"),
        );
        let expected = crate::tools::ToolModelSelection {
            provider: "openrouter".to_string(),
            model: "vendor/fallback-model".to_string(),
        };
        assert_eq!(standalone_scope.current(), Some(expected.clone()));
        assert_eq!(old_snapshot.model_scope().current(), Some(expected.clone()));
        assert_eq!(shared.snapshot().model_scope().current(), Some(expected));
        assert_eq!(captured.provider, "silent-provider");
        assert_eq!(captured.model, "test-model");
        assert_eq!(
            other.shared_tools().snapshot().model_scope().current(),
            Some(captured.clone()),
            "another registry must retain its own parent model"
        );

        agent.set_provider(Arc::new(SilentProvider));
        assert_eq!(standalone_scope.current(), Some(captured));
    }

    struct ModelScopeProbeTool {
        name: &'static str,
        bindings: StdArc<StdTestMutex<Vec<crate::tools::ToolModelScope>>>,
    }

    #[async_trait]
    impl crate::tools::Tool for ModelScopeProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn label(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            self.name
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn bind_model_scope(&mut self, scope: crate::tools::ToolModelScope) {
            self.bindings.lock().unwrap().push(scope);
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
        ) -> Result<crate::tools::ToolOutput> {
            panic!("model affinity binding must not execute a tool");
        }
    }

    #[test]
    fn newly_mounted_and_replaced_tools_share_the_live_model_scope() {
        let bindings = StdArc::new(StdTestMutex::new(Vec::new()));
        let probe = |name| -> Box<dyn crate::tools::Tool> {
            Box::new(ModelScopeProbeTool {
                name,
                bindings: StdArc::clone(&bindings),
            })
        };
        let tools = ToolRegistry::from_tools(vec![probe("initial")]);
        let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
        let shared = agent.shared_tools();
        let old_snapshot = shared.snapshot();
        shared.update(|registry| {
            registry.push(probe("pushed"));
            registry.extend(vec![probe("extended")]);
        });

        let target = crate::models::ad_hoc_model_entry("openai", "next-model").unwrap();
        agent.set_provider(crate::providers::create_provider(&target, None).unwrap());
        shared.update(|registry| {
            registry.push(probe("pushed"));
            registry.extend(vec![probe("extended"), probe("late")]);
        });

        assert_eq!(old_snapshot.tools().len(), 1);
        assert_eq!(shared.snapshot().tools().len(), 4);
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 6, "initial, mounted, and replacement tools");
        for scope in bindings.iter() {
            assert_eq!(
                scope.current(),
                Some(crate::tools::ToolModelSelection {
                    provider: "openai".to_string(),
                    model: "next-model".to_string(),
                })
            );
        }
        agent.set_provider(Arc::new(SilentProvider));
        for scope in bindings.iter() {
            assert_eq!(scope.current().unwrap().provider, "silent-provider");
        }
    }

    #[test]
    fn set_provider_model_records_model_change_once() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "anthropic-key".to_string(),
                },
            );
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );

            let mut agent_session = build_switch_test_session(&auth);
            let model_scope = agent_session.agent.shared_tools().snapshot().model_scope();
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("switch model");
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("repeat same model");
            assert_eq!(
                model_scope.current(),
                Some(crate::tools::ToolModelSelection {
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                })
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            let model_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| matches!(entry, crate::session::SessionEntry::ModelChange(_)))
                .count();
            assert_eq!(model_changes, 1);
        });
    }

    #[test]
    fn set_provider_model_persists_canonical_registry_identity() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );

            let mut agent_session = build_switch_test_session(&auth);
            agent_session
                .set_provider_model(" OpenAI ", "GPT-4O")
                .await
                .expect("mixed-case alias should resolve");

            assert_eq!(agent_session.agent.provider().name(), "openai");
            assert_eq!(agent_session.agent.provider().model_id(), "gpt-4o");
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("openai"));
            assert_eq!(session.header.model_id.as_deref(), Some("gpt-4o"));
            assert_eq!(
                session.effective_model_for_current_path(),
                Some(("openai".to_string(), "gpt-4o".to_string()))
            );
        });
    }

    #[test]
    fn model_and_thinking_transitions_reopen_with_complete_state() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );
            let mut agent_session = build_switch_test_session(&auth);
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                let mut persistent = Session::create_with_dir(Some(dir.path().join("sessions")));
                persistent.header.provider = Some("anthropic".to_string());
                persistent.header.model_id = Some("claude-sonnet-4-5".to_string());
                *session = persistent;
            }
            agent_session.save_enabled = true;

            agent_session
                .set_provider_model("openai", "gpt-5.5")
                .await
                .expect("persist model transition");
            agent_session
                .set_thinking_level(crate::model::ThinkingLevel::High)
                .await
                .expect("persist thinking transition");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            let path = session.path.clone().expect("persisted session path");
            drop(session);
            let reopened = Session::open(path.to_string_lossy().as_ref())
                .await
                .expect("reopen persisted transitions");
            assert_eq!(reopened.header.provider.as_deref(), Some("openai"));
            assert_eq!(reopened.header.model_id.as_deref(), Some("gpt-5.5"));
            assert_eq!(reopened.header.thinking_level.as_deref(), Some("high"));
            assert_eq!(
                reopened.effective_model_for_current_path(),
                Some(("openai".to_string(), "gpt-5.5".to_string()))
            );
            assert_eq!(
                reopened
                    .effective_thinking_level_for_current_path()
                    .as_deref(),
                Some("high")
            );
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_and_normalizes_thinking() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_current_thinking_when_header_omits_it() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = None;
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_rejects_missing_credentials() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("openai".to_string());
                session.header.model_id = Some("gpt-4o".to_string());
            }

            let err = agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect_err("sync should reject switching to a credentialed target without a key");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("openai"));
            assert_eq!(session.header.model_id.as_deref(), Some("gpt-4o"));
        });
    }

    #[test]
    fn sync_runtime_selection_quarantines_failed_normalization_without_live_mutation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );
            let mut agent_session = build_switch_test_session(&auth);
            let blocked_path = dir.path().join("blocked.jsonl");
            std::fs::create_dir_all(&blocked_path).expect("create blocking directory");
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.path = Some(blocked_path);
                session.header.provider = Some("openai".to_string());
                session.header.model_id = Some("GPT-4O".to_string());
                session.header.thinking_level = Some("high".to_string());
            }
            agent_session.save_enabled = true;
            let original_provider = agent_session.agent.provider();
            let original_options = agent_session.agent.stream_options().clone();

            let err = agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect_err("unwritable normalization must fail closed");
            assert!(err.is_session_persistence(), "unexpected error: {err}");
            assert_eq!(
                agent_session.agent.provider().name(),
                original_provider.name()
            );
            assert_eq!(
                agent_session.agent.provider().model_id(),
                original_provider.model_id()
            );
            assert_eq!(
                agent_session.agent.stream_options().api_key,
                original_options.api_key
            );
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                original_options.thinking_level
            );
            assert!(agent_session.provider_admission.reason().is_some());

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("openai"));
            assert_eq!(session.header.model_id.as_deref(), Some("GPT-4O"));
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
            drop(session);

            let compact_err = agent_session
                .compact_now(|_| {})
                .await
                .expect_err("compaction must honor transition quarantine");
            assert!(compact_err.is_session_persistence());
            let extension_err = agent_session
                .execute_extension_command("unused", "", 1, |_| {})
                .await
                .expect_err("extension execution must honor transition quarantine");
            assert!(extension_err.is_session_persistence());
        });
    }

    #[test]
    fn set_provider_model_allows_current_model_without_registry() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            agent_session.model_registry = None;
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            agent_session
                .set_provider_model("anthropic", "claude-sonnet-4-5")
                .await
                .expect("re-persisting the current model should succeed without a registry");

            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::High)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
        });
    }

    #[test]
    fn auto_compaction_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({
                "summary": "Compacted",
                "firstKeptEntryId": "abc123",
                "tokensBefore": 50000,
                "details": { "readFiles": [], "modifiedFiles": [] }
            })),
            aborted: false,
            will_retry: true,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json["result"].is_object());
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], true);
        assert!(json.get("errorMessage").is_none());
    }

    #[test]
    fn auto_compaction_end_with_error_serializes_error_message() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json.get("result").is_none());
        assert_eq!(json["errorMessage"], "compaction failed");
    }

    #[test]
    fn apply_compaction_result_emits_structured_result_payload() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            let result = compaction::CompactionResult {
                summary: "Compacted 10 messages into 2".to_string(),
                first_kept_entry_id: "entry-5".to_string(),
                tokens_before: 12_000,
                details: compaction::CompactionDetails {
                    read_files: vec!["src/main.rs".to_string()],
                    modified_files: vec!["src/agent.rs".to_string()],
                    mode: None,
                },
                snap_payload: None,
            };
            let provider_admission = agent_session
                .provider_admission
                .acquire(&asupersync::Cx::for_testing())
                .await
                .expect("provider admission");

            agent_session
                .apply_compaction_result(result, on_event, provider_admission, None, None)
                .await
                .expect("apply compaction result");

            let payload = {
                let guard = events.lock().expect("lock captured events");
                guard
                    .iter()
                    .find_map(|event| match event {
                        AgentEvent::AutoCompactionEnd {
                            result: Some(result),
                            ..
                        } => Some(result.clone()),
                        _ => None,
                    })
                    .expect("auto compaction end payload")
            };

            assert_eq!(payload["summary"], "Compacted 10 messages into 2");
            assert_eq!(payload["firstKeptEntryId"], "entry-5");
            assert_eq!(payload["tokensBefore"], 12_000);
            // tokensAfter is the estimated post-compaction context: additive,
            // present, positive, and strictly smaller than tokensBefore (the
            // whole point of compaction — it shrinks the context). It is the
            // char-based heuristic over the post-compaction current path, not
            // the hand-set tokens_before.
            let tokens_after = payload["tokensAfter"]
                .as_u64()
                .expect("tokensAfter present and integer");
            assert!(tokens_after > 0, "tokensAfter must be positive");
            assert!(
                tokens_after < 12_000,
                "tokensAfter ({tokens_after}) must be < tokensBefore"
            );
            assert_eq!(payload["details"]["readFiles"], json!(["src/main.rs"]));
            assert_eq!(payload["details"]["modifiedFiles"], json!(["src/agent.rs"]));
        });
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn session_compact_hook_can_reenter_provider_after_transition_install() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("compaction-reentry.mjs");
            std::fs::write(
                &entry_path,
                r#"
                import { complete } from "@mariozechner/pi-ai";

                export default function init(pi) {
                  pi.on("session_compact", async () => {
                    await complete(
                      { id: "capture-model" },
                      [{ role: "user", content: "after compaction" }],
                      { maxTokens: 16 }
                    );
                  });
                }
                "#,
            )
            .expect("write compaction extension");

            let calls = Arc::new(StdMutex::new(Vec::new()));
            let provider = Arc::new(PiAiCaptureProvider {
                calls: Arc::clone(&calls),
            });
            let tools = ToolRegistry::new(&[], temp_dir.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable compaction extension");

            let result = compaction::CompactionResult {
                summary: "Compacted before provider re-entry".to_string(),
                first_kept_entry_id: "entry-5".to_string(),
                tokens_before: 12_000,
                details: compaction::CompactionDetails {
                    read_files: Vec::new(),
                    modified_files: Vec::new(),
                    mode: None,
                },
                snap_payload: None,
            };
            let provider_admission = agent_session
                .provider_admission
                .acquire(&asupersync::Cx::for_testing())
                .await
                .expect("provider admission");
            let on_event: AgentEventHandler = Arc::new(|_| {});

            let outcome = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                agent_session.apply_compaction_result(
                    result,
                    on_event,
                    provider_admission,
                    None,
                    None,
                ),
            )
            .await
            .expect("session_compact hook must not deadlock on provider admission");
            outcome.expect("apply compaction result");

            let captured = calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                captured.len(),
                1,
                "post-install session_compact hook must reach the provider exactly once"
            );
            assert!(matches!(
                captured[0].messages.as_slice(),
                [Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                })] if text == "after compaction"
            ));
        });
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn compaction_persistence_failure_preserves_live_session_and_quarantines_provider() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let blocked_root = tempfile::tempdir().expect("tempdir");
            let blocked_session_dir = blocked_root.path().join("not-a-directory");
            std::fs::write(&blocked_session_dir, b"blocked").expect("write path blocker");

            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                blocked_session_dir,
            ))));
            let agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );
            let metrics_before = {
                let cx = asupersync::Cx::for_testing();
                session
                    .lock(&cx)
                    .await
                    .expect("session lock")
                    .autosave_metrics()
            };

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });
            let result = compaction::CompactionResult {
                summary: "private candidate must not leak".to_string(),
                first_kept_entry_id: "entry-5".to_string(),
                tokens_before: 12_000,
                details: compaction::CompactionDetails {
                    read_files: Vec::new(),
                    modified_files: Vec::new(),
                    mode: None,
                },
                snap_payload: None,
            };
            let provider_admission = agent_session
                .provider_admission
                .acquire(&asupersync::Cx::for_testing())
                .await
                .expect("provider admission");

            let err = agent_session
                .apply_compaction_result(result, on_event, provider_admission, None, None)
                .await
                .expect_err("blocked persistence must fail");
            assert!(
                err.to_string()
                    .contains("compaction persistence remained indeterminate"),
                "unexpected persistence error: {err}"
            );
            assert!(
                agent_session.provider_admission.reason().is_some_and(
                    |reason| reason.contains("compaction persistence remained indeterminate")
                ),
                "indeterminate persistence must quarantine provider re-entry"
            );

            let cx = asupersync::Cx::for_testing();
            let session = session.lock(&cx).await.expect("session lock after failure");
            assert!(
                session
                    .entries_for_current_path()
                    .iter()
                    .all(|entry| !matches!(
                        entry,
                        crate::session::SessionEntry::Compaction(compaction)
                            if compaction.summary == "private candidate must not leak"
                    )),
                "failed private candidate must not be installed into the live session"
            );
            let metrics_after = session.autosave_metrics();
            assert_eq!(
                metrics_after.pending_mutations, metrics_before.pending_mutations,
                "failed private candidate must not alter the live autosave queue"
            );
            assert_eq!(
                metrics_after.flush_started, metrics_before.flush_started,
                "candidate flush attempts must remain private"
            );

            let events = events.lock().expect("lock compaction events");
            assert_eq!(events.len(), 1, "failure must emit one terminal event");
            assert!(matches!(
                &events[0],
                AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(message),
                } if message.contains("compaction persistence remained indeterminate")
            ));
        });
    }

    #[test]
    fn maybe_compact_forces_local_compaction_when_quota_blocked_and_oversized() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));

            // Small window so a handful of messages is "catastrophically"
            // oversized (>= 2x window) by heuristic estimation (chars / 3).
            let settings = ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 100,
                reserve_tokens: 10,
                keep_recent_tokens: 30,
                mode: compaction::AutoCompactionMode::default(),
                render_mode: compaction::CompactionRenderMode::default(),
            };

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                for i in 0..6 {
                    guard.append_message(crate::session::SessionMessage::User {
                        content: UserContent::Text(format!(
                            "oversized turn {i}: {}",
                            "x".repeat(300)
                        )),
                        timestamp: Some(0),
                    });
                }
            }

            let mut agent_session = AgentSession::new(agent, session, false, settings);
            // Exhaust the per-session attempt quota so the background worker is
            // permanently blocked (the deadlock reported for oversized sessions).
            agent_session
                .compaction_worker
                .set_attempt_count_for_test(u32::MAX);
            assert!(!agent_session.compaction_worker.can_start());

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            agent_session
                .maybe_compact(on_event)
                .await
                .expect("maybe_compact");

            let (start_reason, end_payload) = {
                let captured = events.lock().expect("lock captured events");
                let start_reason = captured.iter().find_map(|event| match event {
                    AgentEvent::AutoCompactionStart { reason } => Some(reason.clone()),
                    _ => None,
                });
                let end_payload = captured.iter().find_map(|event| match event {
                    AgentEvent::AutoCompactionEnd {
                        result: Some(result),
                        ..
                    } => Some(result.clone()),
                    _ => None,
                });
                drop(captured);
                (start_reason, end_payload)
            };
            let start_reason = start_reason.expect("forced local compaction should start");
            assert!(
                start_reason.starts_with("forced_local"),
                "unexpected start reason: {start_reason}"
            );
            let end_payload = end_payload.expect("forced local compaction should complete");
            assert!(
                end_payload["summary"]
                    .as_str()
                    .expect("summary string")
                    .contains("deterministic fallback")
            );

            // The session must now contain a compaction entry.
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert!(
                guard
                    .entries_for_current_path()
                    .iter()
                    .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))),
                "forced local compaction should append a compaction entry"
            );
        });
    }

    #[test]
    fn maybe_compact_quota_blocked_is_noop_below_forced_threshold() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));

            // Over the compaction threshold (window - reserve) but below the
            // forced-local threshold (2x window): ~600 chars => ~200 tokens.
            let settings = ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 150,
                reserve_tokens: 10,
                keep_recent_tokens: 30,
                mode: compaction::AutoCompactionMode::default(),
                render_mode: compaction::CompactionRenderMode::default(),
            };

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                for i in 0..2 {
                    guard.append_message(crate::session::SessionMessage::User {
                        content: UserContent::Text(format!("turn {i}: {}", "x".repeat(300))),
                        timestamp: Some(0),
                    });
                }
            }

            let mut agent_session = AgentSession::new(agent, session, false, settings);
            agent_session
                .compaction_worker
                .set_attempt_count_for_test(u32::MAX);

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            agent_session
                .maybe_compact(on_event)
                .await
                .expect("maybe_compact");

            assert!(
                events.lock().expect("lock captured events").is_empty(),
                "quota-blocked compaction below the forced threshold must stay a no-op"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert!(
                !guard
                    .entries_for_current_path()
                    .iter()
                    .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))),
            );
        });
    }

    #[test]
    fn auto_retry_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 4000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 4000);
        assert_eq!(json["errorMessage"], "rate limited");
    }

    #[test]
    fn auto_retry_end_success_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_failure_serializes_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "max retries exceeded");
    }

    // === Owned background privacy through native invocation context ===

    struct CapturedInvocationPrivacy {
        has_host: bool,
        snapshot: crate::text_completion::AuxiliaryPrivacySnapshot,
    }

    struct InvocationPrivacyProbeTool {
        name: &'static str,
        captures: Arc<StdMutex<Vec<CapturedInvocationPrivacy>>>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl crate::tools::Tool for InvocationPrivacyProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn label(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Capture the owned privacy policy of one native invocation"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            Err(Error::tool(self.name, "invocation context was not supplied"))
        }

        async fn execute_with_invocation(
            &self,
            _tool_call_id: &str,
            _input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
            context: crate::tools::ToolInvocationContext<'_>,
        ) -> Result<ToolOutput> {
            let has_host = context.host.is_some();
            self.captures.lock().unwrap().push(CapturedInvocationPrivacy { // ubs:ignore serialized test capture must record the invocation snapshot
                has_host,
                snapshot: (context.snapshot_privacy)(),
            });
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("owned privacy captured"))],
                details: Some(json!({"hasHost": has_host})),
                is_error: false,
            })
        }
    }

    fn remember_invocation_secret(agent: &mut Agent, secret: &str) {
        // The fixture must depend on remembered session state, not merely
        // trigger the generic credential detector again in its bare form.
        assert_eq!( // ubs:ignore fixture proves this opaque value is not rediscovered by generic credential matching
            crate::text_completion::redact_inputs(
                &[secret],
                &crate::text_completion::AuxiliaryPrivacy::default(),
            )
            .unwrap(), // ubs:ignore generic projection must succeed before testing remembered-secret privacy
            [secret],
        );
        let assignment = agent // ubs:ignore fixture must seed the owning Agent's remembered credential
            .secrets_transform_outbound_text(&format!("API_KEY={secret}"))
            .expect("remember opaque credential in the owning session"); // ubs:ignore remembered-credential fixture setup must succeed
        assert!(!assignment.contains(secret)); // ubs:ignore assert the Agent learned and masked the fixture credential
    }

    fn assert_invocation_snapshot_masks(
        snapshot: &crate::text_completion::AuxiliaryPrivacySnapshot,
        secret: &str,
    ) {
        let projected = snapshot // ubs:ignore retained snapshot must remain usable after Agent destruction
            .project_json(&json!({"echo": secret, "nested": [secret, "ordinary text"]}))
            .expect("project after the owning Agent was dropped"); // ubs:ignore successful post-drop projection is the lifecycle behavior under test
        assert_eq!(projected, json!({ // ubs:ignore assert exact privacy projection of content and nested details
            "echo": "<pi-secret:redacted>",
            "nested": ["<pi-secret:redacted>", "ordinary text"],
        }));
    }

    #[test]
    fn native_invocation_privacy_is_owned_and_fresh_for_each_call() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap(); // ubs:ignore native invocation fixture requires its async runtime
        runtime.block_on(async {
            let captures = Arc::new(StdMutex::new(Vec::new()));
            let tools = ToolRegistry::from_tools(vec![Box::new(InvocationPrivacyProbeTool {
                name: "privacy_probe",
                captures: Arc::clone(&captures),
            })]);
            let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
            let first = "rememberedCredentialValue123456";
            let later = "anotherCredentialValue654321";
            for (index, secret) in [first, later].into_iter().enumerate() {
                remember_invocation_secret(&mut agent, secret);
                let (output, failed) = agent
                    .execute_tool(
                        ToolCall {
                            id: format!("capture-{index}"),
                            name: "privacy_probe".to_string(),
                            arguments: json!({}),
                            thought_signature: None,
                        },
                        Arc::new(|_| {}),
                        Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                    )
                    .await;
                assert!(!failed && !output.is_error, "{output:?}"); // ubs:ignore assert both calls reach the invocation-aware path successfully
            }
            drop(agent);
            let captures = captures.lock().unwrap(); // ubs:ignore inspect the completed fixture captures after Agent destruction
            let [first_capture, later_capture] = captures.as_slice() else {
                panic!("both native calls must reach the invocation entry point"); // ubs:ignore fail the regression if either invocation bypasses the new context seam
            };
            assert!(first_capture.has_host && later_capture.has_host); // ubs:ignore direct native invocations must receive scoped host authority
            assert_invocation_snapshot_masks(&first_capture.snapshot, first);
            assert_invocation_snapshot_masks(&later_capture.snapshot, first);
            assert_invocation_snapshot_masks(&later_capture.snapshot, later);
            assert_eq!( // ubs:ignore assert the first snapshot never shares subsequently learned secrets
                first_capture.snapshot.project_json(&json!(later)).unwrap(), // ubs:ignore clean later value must project through the earlier immutable snapshot
                json!(later),
                "an admitted task must not share the Agent's subsequently mutated vault",
            );
            drop(captures);
        });
    }

    #[test]
    fn discoverable_invocation_retains_owning_agent_privacy_after_dispatch() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap(); // ubs:ignore xdev invocation fixture requires its async runtime
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap(); // ubs:ignore discoverable registry fixture requires an isolated workspace
            let captures = Arc::new(StdMutex::new(Vec::new()));
            let mut tools = ToolRegistry::new(&["ast_grep"], root.path(), None);
            tools.push(Box::new(InvocationPrivacyProbeTool {
                name: "ast_grep",
                captures: Arc::clone(&captures),
            }));
            assert!(tools.is_discoverable("ast_grep")); // ubs:ignore assert the fixture really exercises xdev discovery
            let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
            let secret = "rememberedCredentialValue123456";
            remember_invocation_secret(&mut agent, secret);
            let (output, failed) = agent
                .execute_tool(
                    xdev_call("run", Some("ast_grep"), Some(json!({}))),
                    Arc::new(|_| {}),
                    Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                )
                .await;
            assert!(!failed && !output.is_error, "{output:?}"); // ubs:ignore assert the discoverable invocation completes successfully
            assert_eq!(output.details.as_ref().unwrap()["dispatchedVia"], "xdev"); // ubs:ignore fixture requires the xdev routing field in the returned details
            drop(agent);
            let capture = captures.lock().unwrap().pop().expect("dispatched invocation"); // ubs:ignore the completed xdev invocation must retain exactly its privacy capture
            assert!(capture.has_host); // ubs:ignore assert xdev forwards invocation-scoped host authority
            assert_invocation_snapshot_masks(&capture.snapshot, secret);
        });
    }

    #[test]
    fn eval_nested_invocation_can_own_privacy_without_retaining_tool_authority() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap(); // ubs:ignore eval integration fixture requires its async runtime
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap(); // ubs:ignore eval fixture requires an isolated kernel workspace
            let captures = Arc::new(StdMutex::new(Vec::new()));
            let mut tools = ToolRegistry::new(&["eval"], root.path(), None);
            tools.push(Box::new(InvocationPrivacyProbeTool {
                name: "privacy_probe",
                captures: Arc::clone(&captures),
            }));
            let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
            let secret = "rememberedCredentialValue123456";
            remember_invocation_secret(&mut agent, secret);
            let (output, failed) = agent
                .execute_tool(
                    eval_host_call("js", r#"
                        const result = await tool.result("privacy_probe", {});
                        if (result.isError || result.details.hasHost !== false) {
                          throw new Error("nested invocation renewed tool-call authority");
                        }
                        "nested-owned-privacy"
                    "#),
                    Arc::new(|_| {}),
                    Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                )
                .await;
            assert!(!failed && !output.is_error, "{output:?}"); // ubs:ignore assert the real nested eval invocation succeeds without renewing authority
            assert!(eval_host_text(&output).contains("nested-owned-privacy")); // ubs:ignore assert the guest completed the host-authority check
            drop(agent);
            let capture = captures.lock().unwrap().pop().expect("nested invocation"); // ubs:ignore the completed nested invocation must supply an owned privacy snapshot
            assert!(!capture.has_host); // ubs:ignore assert nested calls cannot retain or renew eval host authority
            assert_invocation_snapshot_masks(&capture.snapshot, secret);
        });
    }

    // === Eval calls through the owning Agent's tool policy ===

    fn eval_host_call(kernel: &str, code: &str) -> ToolCall {
        ToolCall {
            id: format!("outer-eval-{kernel}"),
            name: "eval".to_string(),
            arguments: json!({"kernel": kernel, "code": code}),
            thought_signature: None,
        }
    }

    fn eval_host_text(output: &ToolOutput) -> String {
        output.content.iter().filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn eval_host_calls_use_live_tools_approvals_and_events_in_both_kernels() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for (kernel, code) in [
                ("js", r#"
                    await tool.call("write", {path:"allowed.txt",content:"native eval write"});
                    const read = await tool.result("read", {path:"allowed.txt"});
                    if (read.isError || !read.content[0].text.includes("native eval write")) {
                      throw new Error("native read did not observe the write");
                    }
                    const denied = await tool.result("write", {path:"blocked.txt",content:"forbidden"});
                    if (!denied.isError || !denied.content[0].text.includes("denied by eval test")) {
                      throw new Error("nested approval was bypassed");
                    }
                    "eval-host-ok"
                "#),
                ("python", r#"
await tool.call("write", {"path":"allowed.txt","content":"native eval write"})
read = await tool.result("read", {"path":"allowed.txt"})
assert not read["isError"] and "native eval write" in read["content"][0]["text"]
denied = await tool.result("write", {"path":"blocked.txt","content":"forbidden"})
assert denied["isError"] and "denied by eval test" in denied["content"][0]["text"]
"eval-host-ok"
                "#),
            ] {
                if kernel == "python" && !std::process::Command::new("python3")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status().is_ok_and(|status| status.success())
                {
                    eprintln!("Skipping Python eval-host case: python3 not available");
                    continue;
                }
                let root = tempfile::tempdir().unwrap();
                let approvals = Arc::new(StdMutex::new(Vec::new()));
                let approval_sink = Arc::clone(&approvals);
                let agent = Agent::new(
                    Arc::new(SilentProvider),
                    ToolRegistry::new(&["eval", "read", "write"], root.path(), None),
                    AgentConfig {
                        tool_approval: Some(Arc::new(move |request| {
                            let denied = request.tool_name == "write"
                                && request.arguments["path"] == "blocked.txt";
                            approval_sink.lock().unwrap().push(request);
                            Box::pin(async move {
                                if denied {
                                    ToolApprovalDecision::deny("denied by eval test")
                                } else {
                                    ToolApprovalDecision::Allow
                                }
                            })
                        })),
                        ..AgentConfig::default()
                    },
                );
                let events = Arc::new(StdMutex::new(Vec::new()));
                let event_sink = Arc::clone(&events);
                let (output, failed) = agent.execute_tool(
                    eval_host_call(kernel, code),
                    Arc::new(move |event| event_sink.lock().unwrap().push(event)),
                    Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                ).await;
                assert!(!failed && !output.is_error, "{kernel}: {output:?}");
                assert!(eval_host_text(&output).contains("eval-host-ok"));
                assert_eq!(std::fs::read_to_string(root.path().join("allowed.txt")).unwrap(), "native eval write");
                assert!(!root.path().join("blocked.txt").exists());
                let calls = output.details.as_ref().unwrap()["toolCalls"]["calls"].as_array().unwrap();
                assert_eq!(calls.len(), 3);
                assert_eq!(calls.iter().map(|call| call["status"].as_str().unwrap()).collect::<Vec<_>>(),
                    ["completed", "completed", "failed"]);
                let ids: BTreeSet<_> = calls.iter().map(|call| call["toolCallId"].as_str().unwrap()).collect();
                assert_eq!(ids.len(), 3, "each nested call needs its own identity");
                assert!(ids.iter().all(|id| id.starts_with("eval-")));
                let approvals = approvals.lock().unwrap();
                assert_eq!(approvals.iter().map(|request| request.tool_name.as_str()).collect::<Vec<_>>(),
                    ["eval", "write", "read", "write"]);
                assert!(approvals.iter().skip(1).all(|request| ids.contains(request.tool_call_id.as_str())));
                drop(approvals);
                let events = events.lock().unwrap();
                assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. })).count(), 3);
                assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. })).count(), 3);
            }
        });
    }

    #[test]
    fn eval_host_disabled_tools_cannot_fall_back_to_standalone_readers() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("data.txt"), "standalone-reader-proof").unwrap();
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::new(&["eval"], root.path(), None),
                AgentConfig::default(),
            );
            let (output, failed) = agent.execute_tool(
                eval_host_call("js", r#"
                    let unavailable = "";
                    try { await tool.call("read", {path:"data.txt"}); }
                    catch (error) { unavailable = String(error); }
                    unavailable
                "#),
                Arc::new(|_| {}),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(!failed, "{output:?}");
            assert!(eval_host_text(&output).contains("EVAL_BRIDGE_UNAVAILABLE"));
            assert_eq!(output.details.as_ref().unwrap()["toolCalls"]["calls"][0]["status"], "failed");

            // Reuse the exact same retained kernel through the standalone
            // API. Its previous Agent host must not remain in the realm.
            let registry = agent.tools.snapshot();
            let eval = registry.get("eval").unwrap();
            let standalone = eval.execute("standalone", json!({
                "kernel":"js", "code":r#"
                    const local = await tool.call("read", {path:"data.txt"});
                    let denied = "";
                    try { await tool.call("write", {path:"blocked.txt",content:"forbidden"}); }
                    catch (error) { denied = String(error); }
                    [local.includes("standalone-reader-proof"), denied.includes("EVAL_BRIDGE_DENIED")]
                "#,
            }), None).await.unwrap();
            assert!(!standalone.is_error, "{standalone:?}");
            assert!(eval_host_text(&standalone).contains("true,true"));
            assert!(!root.path().join("blocked.txt").exists());
        });
    }

    #[test]
    fn eval_host_rejects_recursive_dispatch_and_bounds_native_calls() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("data.txt"), "bounded").unwrap();
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::new(&["eval", "read"], root.path(), None),
                AgentConfig::default(),
            );
            let (output, failed) = agent.execute_tool(
                eval_host_call("js", r#"
                    const questions = {safe:{type:"bool",instructions:"Is this safe?"}};
                    let unboundBatch = "";
                    try { await tool.call("__pi_judge_batch", {states:[{value:1}],questions}); }
                    catch (error) { unboundBatch = String(error); }
                    if (!unboundBatch.includes("PI_JUDGE_UNBOUND")) {
                      throw new Error("raw judgment batch did not reach its own policy");
                    }
                    const refusals = [];
                    for (const [name,args] of [
                      ["eval",{kernel:"js",code:"42"}],
                      ["xdev",{action:"run",name:"eval",args:{kernel:"js",code:"42"}}],
                      ["xdev",{action:"run",name:"xdev",args:{action:"run",name:"eval"}}],
                      ["__pi_tool_result",{name:"eval",args:{kernel:"js",code:"42"}}],
                      ["__pi_judge",{state:{value:1},questions}],
                      ["__pi_judge_batch",{states:[{value:1}],questions}]
                    ]) {
                      try { await tool.result(name,args); }
                      catch (error) { refusals.push(String(error)); }
                    }
                    if (refusals.length !== 6 || refusals.some(x => !x.includes("EVAL_BRIDGE_RECURSION"))) {
                      throw new Error("recursive dispatch was admitted");
                    }
                    for (let i = 0; i < 58; i++) {
                      const result = await tool.result("read",{path:"data.txt"});
                      if (result.isError) throw new Error("native read failed");
                    }
                    let limit = "";
                    try { await tool.call("read",{path:"data.txt"}); }
                    catch (error) { limit = String(error); }
                    limit
                "#),
                Arc::new(|_| {}),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(!failed, "{output:?}");
            assert!(eval_host_text(&output).contains("EVAL_BRIDGE_LIMIT"));
            let calls = output.details.as_ref().unwrap()["toolCalls"]["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 64);
            assert_eq!(calls.iter().filter(|call| call["status"] == "completed").count(), 58);
        });
    }

    #[test]
    fn eval_host_rechecks_plan_state_before_each_nested_action() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("data.txt"), "planning transition").unwrap();
            let approvals = Arc::new(StdMutex::new(Vec::new()));
            let approval_sink = Arc::clone(&approvals);
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::new(&["eval", "read", "write"], root.path(), None),
                AgentConfig {
                    tool_approval: Some(Arc::new(move |request| {
                        approval_sink.lock().unwrap().push(request.tool_name);
                        Box::pin(async { ToolApprovalDecision::Allow })
                    })),
                    ..AgentConfig::default()
                },
            );
            let plan = agent.plan_state.clone();
            let (output, failed) = agent.execute_tool(
                eval_host_call("js", r#"
                    await tool.call("read", {path:"data.txt"});
                    const blocked = await tool.result("write",{path:"blocked.txt",content:"forbidden"});
                    if (!blocked.isError) throw new Error("plan gate bypassed");
                    blocked.content[0].text
                "#),
                Arc::new(move |event| {
                    if matches!(event, AgentEvent::ToolExecutionEnd { tool_name, .. } if tool_name == "read") {
                        plan.enter_planning();
                    }
                }),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(!failed, "{output:?}");
            assert!(eval_host_text(&output).contains("PLAN_MODE_BLOCKED"));
            assert!(!root.path().join("blocked.txt").exists());
            assert_eq!(*approvals.lock().unwrap(), ["eval", "read"]);
        });
    }

    #[test]
    fn eval_host_session_switch_during_approval_prevents_hooks_and_native_execution() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let extension = root.path().join("observe-nested-hook.mjs");
            std::fs::write(&extension, r#"
                import { complete } from "@mariozechner/pi-ai";
                export default function init(pi) {
                  pi.on("tool_call", async event => {
                    if (event.toolName === "write") {
                      await complete({id:"capture-model"},
                        [{role:"user",content:"a replaced session must not run this hook"}],
                        {maxTokens:16});
                    }
                  });
                }
            "#).unwrap();
            let provider_calls = Arc::new(StdMutex::new(Vec::new()));
            let agent = Agent::new(
                Arc::new(PiAiCaptureProvider { calls: Arc::clone(&provider_calls) }),
                ToolRegistry::new(&["eval", "write"], root.path(), None),
                AgentConfig::default(),
            );
            let mut session = AgentSession::new(
                agent, Arc::new(Mutex::new(Session::in_memory())), false,
                ResolvedCompactionSettings::default(),
            );
            session.enable_extensions(&[], root.path(), None, &[extension]).await.unwrap();
            let scope = session.agent.job_session_scope.clone();
            session.agent.config.tool_approval = Some(Arc::new(move |request| {
                if request.tool_name == "write" {
                    scope.bind(Arc::new(|| Box::pin(async { Some("replacement-session".to_string()) })));
                }
                Box::pin(async { ToolApprovalDecision::Allow })
            }));
            let (output, failed) = session.agent.execute_tool(
                eval_host_call("js", "await tool.call('write',{path:'blocked.txt',content:'forbidden'})"),
                Arc::new(|_| {}),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(failed && output.is_error, "{output:?}");
            assert!(eval_host_text(&output).contains("EVAL_SESSION_CHANGED"));
            assert!(!root.path().join("blocked.txt").exists());
            assert!(provider_calls.lock().unwrap().is_empty(), "a replacement session must not run the nested tool hook");
        });
    }

    #[test]
    fn eval_host_discoverable_eval_keeps_agent_authority() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let config = crate::config::Config {
                tools: Some(crate::config::ToolSettings {
                    load_mode: Some(HashMap::from([("eval".to_string(), "discoverable".to_string())])),
                }),
                ..crate::config::Config::default()
            };
            let tools = ToolRegistry::new(&["eval", "write"], root.path(), Some(&config));
            assert!(tools.is_discoverable("eval"));
            let agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig {
                tool_approval: Some(Arc::new(|request| Box::pin(async move {
                    if request.tool_name == "write" {
                        ToolApprovalDecision::deny("nested xdev approval")
                    } else {
                        ToolApprovalDecision::Allow
                    }
                }))),
                ..AgentConfig::default()
            });
            let (output, failed) = agent.execute_tool(
                xdev_call("run", Some("eval"), Some(json!({
                    "kernel":"js",
                    "code":"(await tool.result('write',{path:'blocked.txt',content:'forbidden'})).content[0].text"
                }))),
                Arc::new(|_| {}),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(!failed && !output.is_error, "{output:?}");
            assert!(eval_host_text(&output).contains("nested xdev approval"));
            assert_eq!(output.details.as_ref().unwrap()["dispatchedVia"], "xdev");
            assert_eq!(output.details.as_ref().unwrap()["toolCalls"]["calls"][0]["status"], "failed");
            assert!(!root.path().join("blocked.txt").exists());
        });
    }

    #[test]
    fn eval_host_first_lookup_cannot_adopt_a_replacement_session() {
        use crate::tools::ToolInvocationHost;

        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::new(&["write"], root.path(), None),
                AgentConfig::default(),
            );
            let original_owner = agent.job_session_scope.session_id().await.unwrap();
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = Arc::clone(&lookups);
            agent.job_session_scope.bind(Arc::new(move || {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    // The replacement is first observed while resolving the
                    // host's first bridge call, after cell ownership froze.
                    observed.fetch_add(1, Ordering::SeqCst);
                    Some("replacement-session".to_string())
                })
            }));
            let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {
                panic!("a replaced session must not admit a nested tool")
            }));
            let call = ToolCall {
                id: "first-host-call".to_string(),
                name: "write".to_string(),
                arguments: json!({"path":"blocked.txt","content":"forbidden"}),
                thought_signature: None,
            };
            let error = host.call_tool(call.clone(), &original_owner).await.unwrap_err();
            assert!(error.to_string().contains("EVAL_SESSION_CHANGED"));
            assert_eq!(lookups.load(Ordering::SeqCst), 1);
            assert!(!root.path().join("blocked.txt").exists());
            agent.job_session_scope.bind(Arc::new(move || {
                let owner = original_owner.clone();
                Box::pin(async move { Some(owner) })
            }));
            let error = host.call_tool(call, "replacement-session").await.unwrap_err();
            assert!(error.to_string().contains("revoked"), "{error}");
            assert_eq!(lookups.load(Ordering::SeqCst), 1, "revocation is sticky");
        });
    }

    #[test]
    fn eval_host_nested_tools_pass_through_extension_hooks() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("data.txt"), "original data").unwrap();
            let extension = root.path().join("eval-policy.mjs");
            std::fs::write(&extension, r#"
                export default function init(pi) {
                  pi.on("tool_call", async event => {
                    if (event.toolName === "write") return {block:true,reason:"nested hook veto"};
                  });
                  pi.on("tool_result", async event => {
                    if (event.toolName === "read") return {
                      content:[{type:"text",text:"nested hook replacement"}],
                      details:{source:"nested result hook"}
                    };
                  });
                }
            "#).unwrap();
            let agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::new(&["eval", "read", "write"], root.path(), None),
                AgentConfig::default(),
            );
            let mut session = AgentSession::new(
                agent, Arc::new(Mutex::new(Session::in_memory())), false,
                ResolvedCompactionSettings::default(),
            );
            session.enable_extensions(&[], root.path(), None, &[extension]).await.unwrap();
            let (output, failed) = session.agent.execute_tool(
                eval_host_call("js", r#"
                    const read = await tool.result("read",{path:"data.txt"});
                    if (read.content[0].text !== "nested hook replacement"
                        || read.details.source !== "nested result hook") throw new Error("missing result hook");
                    const write = await tool.result("write",{path:"blocked.txt",content:"forbidden"});
                    if (!write.isError || !write.content[0].text.includes("nested hook veto")) {
                      throw new Error("missing call hook");
                    }
                    "nested-hooks-ok"
                "#),
                Arc::new(|_| {}),
                Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
            ).await;
            assert!(!failed && !output.is_error, "{output:?}");
            assert!(eval_host_text(&output).contains("nested-hooks-ok"));
            assert!(!root.path().join("blocked.txt").exists());
        });
    }

    // === Tool load modes / xdev dispatcher (bd-cv653.1.6) ===

    fn xdev_test_agent(enabled: &[&str], cwd: &Path) -> Agent {
        let provider = Arc::new(SilentProvider);
        let tools = ToolRegistry::new(enabled, cwd, None);
        Agent::new(provider, tools, AgentConfig::default())
    }

    fn xdev_call(action: &str, name: Option<&str>, args: Option<Value>) -> ToolCall {
        let mut arguments = json!({ "action": action });
        if let Some(name) = name {
            arguments["name"] = json!(name);
        }
        if let Some(inner) = args {
            arguments["args"] = inner;
        }
        ToolCall {
            id: "call-xdev-1".to_string(),
            name: "xdev".to_string(),
            arguments,
            thought_signature: None,
        }
    }

    #[test]
    fn xdev_list_and_describe_via_tool_execute() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let registry = agent.tools.snapshot();
            let xdev = registry.get("xdev").expect("xdev registered");

            let list = xdev
                .execute("c1", json!({"action": "list"}), None)
                .await
                .expect("list");
            let list_text = match &list.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                list_text.contains("ast_grep"),
                "list shows discoverable ast_grep"
            );
            assert!(!list.is_error);

            let describe = xdev
                .execute(
                    "c2",
                    json!({"action": "describe", "name": "ast_grep"}),
                    None,
                )
                .await
                .expect("describe");
            let describe_text = match &describe.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            let parsed: Value = serde_json::from_str(&describe_text).expect("describe JSON");
            assert_eq!(parsed["name"], "ast_grep");
            assert!(parsed["parameters"]["properties"]["pattern"].is_object());
        });
    }

    #[test]
    fn xdev_run_executes_discoverable_tool_through_agent() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                temp.path().join("main.rs"),
                "fn main() { let _ = compute().unwrap(); }\nfn compute() -> i32 { 1 }\n",
            )
            .expect("write fixture");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let call = xdev_call(
                "run",
                Some("ast_grep"),
                Some(json!({"pattern": "$EXPR.unwrap()", "path": "."})),
            );
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                .await;
            assert!(!is_error, "run must succeed: {:?}", output.content);
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                text.contains("main.rs"),
                "run output names the fixture file: {text}"
            );
            assert_eq!(
                output
                    .details
                    .as_ref()
                    .and_then(|d| d.get("dispatchedVia"))
                    .and_then(Value::as_str),
                Some("xdev")
            );
        });
    }

    #[test]
    fn xdev_promote_moves_tool_into_live_schema() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut agent = xdev_test_agent(&["read", "ast_grep"], temp.path());

            // Before promotion: ast_grep is discoverable, so the schema omits it.
            let defs_before = agent
                .build_context()
                .tools
                .iter()
                .map(|def| def.name.clone())
                .collect::<Vec<_>>();
            assert!(defs_before.contains(&"read".to_string()));
            assert!(defs_before.contains(&"xdev".to_string()));
            assert!(
                !defs_before.contains(&"ast_grep".to_string()),
                "discoverable tool must be hidden pre-promotion"
            );

            let call = xdev_call("promote", Some("ast_grep"), None);
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                .await;
            assert!(!is_error);

            let has_promoted_tool = agent
                .build_context()
                .tools
                .iter()
                .any(|def| def.name == "ast_grep");
            assert!(
                has_promoted_tool,
                "promoted tool enters the schema mid-session"
            );
        });
    }

    #[test]
    fn xdev_run_unknown_tool_named_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let call = xdev_call("run", Some("nosuch_tool"), Some(json!({})));
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                .await;
            assert!(is_error, "unknown discoverable tool must error");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("nosuch_tool"), "error names the tool: {text}");
        });
    }

    #[test]
    fn xdev_schema_token_accounting() {
        // Bead evidence: schema-token delta essential-only vs all-tools.
        let temp = tempfile::tempdir().expect("tempdir");
        let full_registry = ToolRegistry::new(
            &[
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "find",
                "ls",
                "hashline_edit",
                "ast_grep",
                "ast_edit",
            ],
            temp.path(),
            None,
        );
        let all_defs: usize = full_registry
            .tools()
            .iter()
            .map(|t| serde_json::to_string(&json!({"name": t.name(), "description": t.description(), "parameters": t.parameters()})).unwrap().len())
            .sum();
        let essential_defs: usize = full_registry
            .tools()
            .iter()
            .filter(|t| !full_registry.is_discoverable(t.name()))
            .map(|t| serde_json::to_string(&json!({"name": t.name(), "description": t.description(), "parameters": t.parameters()})).unwrap().len())
            .sum();
        println!(
            "xdev token accounting: all={all_defs} bytes, essential={essential_defs} bytes, saved={}",
            all_defs - essential_defs
        );
        assert!(
            essential_defs < all_defs,
            "essential schema must be strictly smaller ({essential_defs} < {all_defs})"
        );
        assert!(full_registry.is_discoverable("ast_grep"));
        assert!(!full_registry.is_discoverable("read"));
        assert!(full_registry.get("xdev").is_some(), "xdev auto-registered");
    }

    #[test]
    fn xdev_dispatch_preserves_result_details_without_routing_key_collisions() {
        let original = json!({
            "schema": "pi.memory.v1",
            "memories": [{"id": 7}],
            "history": {"status": "unavailable", "diagnostic": "CASS_NOT_INSTALLED"},
            "resultDetails": {"existing": true}
        });
        let details = Agent::xdev_dispatch_details("recall", Some(original.clone()));
        for (key, value) in original.as_object().unwrap() {
            assert_eq!(details.get(key), Some(value));
        }
        assert_eq!(details["dispatchedVia"], "xdev");
        assert_eq!(details["tool"], "recall");

        for original in [
            Value::Null,
            json!(42),
            json!(["evidence"]),
            json!("plain metadata"),
            json!({"tool": "inner-tool", "resultDetails": {"existing": true}}),
            json!({"dispatchedVia": "original-route", "history": {"status": "completed"}}),
        ] {
            let details = Agent::xdev_dispatch_details("recall", Some(original.clone()));
            assert_eq!(details["resultDetails"], original);
            assert_eq!(details["dispatchedVia"], "xdev");
            assert_eq!(details["tool"], "recall");
        }
        assert_eq!(
            Agent::xdev_dispatch_details("recall", None),
            json!({"dispatchedVia": "xdev", "tool": "recall"})
        );
    }

    // === Plan mode (bd-cv653.3.5) ===

    fn cass_recall_plan_test_agent(cwd: &Path, through_xdev: bool) -> Agent {
        let config: crate::config::Config =
            serde_json::from_value(json!({"memory": {"backend": "cass"}})).unwrap();
        let mut tools = ToolRegistry::new(&[], cwd, Some(&config));
        assert!(tools.is_discoverable("recall"));
        if !through_xdev {
            tools.mark_promoted("recall");
        }
        tools.bind_job_session_resolver(Arc::new(|| {
            Box::pin(async { Some("cass-plan-owner".to_string()) })
        }));
        let store = Arc::new(crate::memory::MemoryStore::open(cwd).unwrap());
        store
            .retain(
                crate::memory::MemoryKind::Fact,
                "parser fact available while planning",
                &[],
                None,
            )
            .unwrap();
        crate::memory::shared::SharedMemoryStore::new(store, "cass-plan-owner")
            .unwrap()
            .write("handoff", "exact planning handoff", None)
            .unwrap();
        let agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
        agent.plan_state().enter_planning();
        agent
    }

    #[test]
    fn plan_gate_cass_recall_keeps_history_and_malformed_inputs_behind_process_barrier() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for through_xdev in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let agent = cass_recall_plan_test_agent(root.path(), through_xdev);
                for input in [
                    None,
                    Some(json!({"query": "parser"})),
                    Some(json!({"query": "parser", "includeHistory": true})),
                    Some(json!({"query": "parser", "includeHistory": null})),
                    Some(json!({"query": "parser", "includeHistory": "false"})),
                    Some(Value::Null),
                    Some(json!([{"includeHistory": false}])),
                ] {
                    let call = if through_xdev {
                        let mut call = xdev_call("run", Some("recall"), input);
                        // Outer dispatcher fields cannot suppress the inner
                        // invocation's process effect or impersonate its scope.
                        call.arguments["includeHistory"] = json!(false);
                        call.arguments["scope"] = json!("session");
                        call
                    } else {
                        ToolCall {
                            id: "cass-blocked".to_string(),
                            name: "recall".to_string(),
                            arguments: input.unwrap_or_else(|| json!({})),
                            thought_signature: None,
                        }
                    };
                    let effects = agent.effects_for_call(&call);
                    assert!(effects.processes(), "history input: {:?}", call.arguments);
                    assert!(!agent.plan_state().allows_effects(effects));
                    // Verify the real gate only after the effect assertions;
                    // this test never dispatches a CASS search.
                    let (output, is_error) = agent
                        .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                        .await;
                    assert!(is_error);
                    let ContentBlock::Text(text) = &output.content[0] else {
                        panic!("plan gate must return an explanation");
                    };
                    assert!(text.text.contains("PLAN_MODE_BLOCKED"));
                }
            }
        });
    }

    #[test]
    fn plan_gate_allows_local_and_shared_recall_directly_and_through_xdev() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for through_xdev in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let agent = cass_recall_plan_test_agent(root.path(), through_xdev);
                for (input, expected_text, details_key) in [
                    (
                        json!({"query": "parser", "includeHistory": false}),
                        "parser fact available while planning",
                        "memories",
                    ),
                    (
                        json!({"scope": "session", "key": "handoff"}),
                        "exact planning handoff",
                        "value",
                    ),
                    (
                        json!({"scope": "session", "limit": 1}),
                        "handoff",
                        "entries",
                    ),
                ] {
                    let call = if through_xdev {
                        xdev_call("run", Some("recall"), Some(input))
                    } else {
                        ToolCall {
                            id: "cass-read-only".to_string(),
                            name: "recall".to_string(),
                            arguments: input,
                            thought_signature: None,
                        }
                    };
                    assert_eq!(agent.effects_for_call(&call), ToolEffects::read());
                    let (output, is_error) = agent
                        .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                        .await;
                    assert!(!is_error, "read-only recall failed: {:?}", output.content);
                    let ContentBlock::Text(text) = &output.content[0] else {
                        panic!("recall must return readable results");
                    };
                    assert!(text.text.contains(expected_text));
                    let details = output.details.unwrap();
                    assert!(details.get(details_key).is_some());
                    assert!(details.get("history").is_none());
                    if through_xdev {
                        assert_eq!(details["dispatchedVia"], "xdev");
                        assert_eq!(details["tool"], "recall");
                    }
                }
            }
        });
    }

    #[test]
    fn plan_gate_blocks_mutation_and_allows_reads() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let target = temp.path().join("scratch.txt");
            std::fs::write(&target, "original").expect("write fixture");

            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(
                &["read", "write", "bash", "grep", "ls", "ast_grep"],
                temp.path(),
                None,
            );
            let agent = Agent::new(provider, tools, AgentConfig::default());
            agent.plan_state().enter_planning();

            // Mutation is blocked with the structured, model-readable error…
            let write_call = ToolCall {
                id: "w1".to_string(),
                name: "write".to_string(),
                arguments: json!({"path": "scratch.txt", "content": "changed"}),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&write_call, Arc::new(|_| {}), None)
                .await;
            assert!(is_error, "write must be blocked while planning");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("PLAN_MODE_BLOCKED"), "gate error: {text}");
            // …and the file is untouched (zero bytes changed).
            assert_eq!(
                std::fs::read_to_string(&target).expect("read back"),
                "original"
            );

            // Reads flow freely.
            let read_call = ToolCall {
                id: "r1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "scratch.txt"}),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&read_call, Arc::new(|_| {}), None)
                .await;
            assert!(!is_error, "read must pass the gate");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("original"), "read returns content: {text}");

            // Bash (process effect) is blocked in plan mode.
            let bash_call = ToolCall {
                id: "b1".to_string(),
                name: "bash".to_string(),
                arguments: json!({"command": "echo hi"}),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&bash_call, Arc::new(|_| {}), None)
                .await;
            assert!(is_error, "bash is blocked while planning");

            // Approval re-opens mutation.
            agent
                .plan_state()
                .submit_plan("goal: test; steps: 1) write".to_string());
            assert!(agent.plan_state().approve().is_some());
            let write_call = ToolCall {
                id: "w2".to_string(),
                name: "write".to_string(),
                arguments: json!({"path": "scratch.txt", "content": "changed"}),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&write_call, Arc::new(|_| {}), None)
                .await;
            assert!(!is_error, "write allowed after approval");
            assert_eq!(
                std::fs::read_to_string(&target).expect("read back"),
                "changed"
            );
        });
    }

    /// bd-wfcu7: the github tool declares a process effect, so plan mode must
    /// refuse it through the real dispatch path before any `gh` is spawned.
    #[cfg(unix)]
    #[test]
    fn plan_gate_refuses_github_before_spawning_gh() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let spawned = temp.path().join("gh-was-spawned");
            let stub = temp.path().join("gh");
            std::fs::write(
                &stub,
                format!("#!/bin/sh\ntouch '{}'\nprintf '[]'\n", spawned.display()),
            )
            .expect("write gh stub");
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod gh stub");
            let tools = ToolRegistry::from_tools(vec![Box::new(crate::github::GithubTool::new(
                temp.path(),
                Some(stub.to_str().expect("utf-8 path")),
            ))]);
            let agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
            agent.plan_state().enter_planning();

            let call = ToolCall {
                id: "gh1".to_string(),
                name: "github".to_string(),
                arguments: json!({"op": "run_list", "repo": "o/r"}),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}), None)
                .await;
            assert!(is_error, "github must be blocked while planning");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("PLAN_MODE_BLOCKED"), "gate error: {text}");
            assert!(
                !spawned.exists(),
                "plan mode must refuse before gh is spawned"
            );
        });
    }

    #[test]
    fn plan_gate_xdev_run_uses_inner_tool_effects() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("fixture");
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(
                &["read", "write", "ast_grep", "ast_edit"],
                temp.path(),
                None,
            );
            let agent = Agent::new(provider, tools, AgentConfig::default());
            agent.plan_state().enter_planning();

            // xdev run on a read-only tool (ast_grep) passes the gate.
            let run_read = ToolCall {
                id: "x1".to_string(),
                name: "xdev".to_string(),
                arguments: json!({
                    "action": "run",
                    "name": "ast_grep",
                    "args": {"pattern": "fn $NAME($$$)", "path": "."}
                }),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&run_read, Arc::new(|_| {}), None)
                .await;
            assert!(
                !is_error,
                "xdev run on a read-only tool passes while planning"
            );

            // xdev run on a mutating tool (ast_edit) is blocked.
            let run_write = ToolCall {
                id: "x2".to_string(),
                name: "xdev".to_string(),
                arguments: json!({
                    "action": "run",
                    "name": "ast_edit",
                    "args": {"ops": [{"pat": "fn main() {}", "out": ""}], "path": "."}
                }),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&run_write, Arc::new(|_| {}), None)
                .await;
            assert!(
                is_error,
                "xdev run on a mutating tool is blocked while planning"
            );
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("PLAN_MODE_BLOCKED"), "gate error: {text}");
        });
    }

    #[test]
    fn plan_mode_session_entries_round_trip() {
        let mut session = Session::in_memory();
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "planning"})));
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "approved"})));
        let json = serde_json::to_string(session.entries_for_current_path()[1]).expect("ser");
        let parsed: crate::session::SessionEntry = serde_json::from_str(&json).expect("reparse");
        let crate::session::SessionEntry::Custom(custom) = &parsed else {
            panic!("expected custom entry");
        };
        assert_eq!(custom.custom_type, "plan_mode");
        assert_eq!(
            custom
                .data
                .as_ref()
                .and_then(|d| d.get("mode"))
                .and_then(Value::as_str),
            Some("approved")
        );
    }

    #[test]
    fn colliding_restored_tool_keys_are_refused_before_approval_and_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let target = temp.path().join("credential.txt");
            std::fs::write(&target, "original").expect("existing file");
            let approvals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed_approvals = Arc::clone(&approvals);
            let config = AgentConfig {
                tool_approval: Some(Arc::new(move |_| {
                    observed_approvals.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { ToolApprovalDecision::Allow })
                })),
                ..AgentConfig::default()
            };
            let tools = ToolRegistry::new(&["write"], temp.path(), None);
            let mut agent = Agent::new(Arc::new(SilentProvider), tools, config);
            let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
            let placeholder = agent
                .secrets_transform_outbound_text(secret)
                .expect("establish credential mapping");
            let mut fields = serde_json::Map::new();
            fields.insert(secret.to_string(), json!("first field"));
            fields.insert(placeholder.clone(), json!("second field"));
            let call = ToolCall {
                id: "ambiguous-write".to_string(),
                name: "write".to_string(),
                arguments: json!({
                    "path": "credential.txt",
                    "content": placeholder,
                    "metadata": [Value::Object(fields)],
                }),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool(
                    call,
                    Arc::new(|_| panic!("refused arguments must not reach approval or tools")),
                    Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                )
                .await;
            assert!(is_error && output.is_error);
            let report = serde_json::to_string(&output).expect("error output");
            assert!(report.contains("PI_SECRET_JSON_RESTORE_KEY_COLLISION"));
            assert!(!report.contains(secret));
            assert_eq!(approvals.load(Ordering::SeqCst), 0);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");

            // The same real tool and vault remain usable after a rejected call.
            let (output, is_error) = agent
                .execute_tool(
                    ToolCall {
                        id: "valid-write".to_string(),
                        name: "write".to_string(),
                        arguments: json!({
                            "path": "credential.txt",
                            "content": placeholder,
                        }),
                        thought_signature: None,
                    },
                    Arc::new(|_| {}),
                    Arc::new(StdMutex::new(TurnLatencyAccumulator::started())),
                )
                .await;
            assert!(!is_error && !output.is_error);
            assert_eq!(approvals.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), secret);
        });
    }

    #[test]
    fn session_state_reset_drops_secret_placeholders_and_plan_gate() {
        let provider = Arc::new(SilentProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());
        let raw_secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let placeholder = agent
            .secrets_transform_outbound_text(raw_secret)
            .expect("obfuscate secret");
        assert_ne!(placeholder, raw_secret);
        agent.plan_state().enter_planning();

        let before_reset = agent
            .restore_secrets_inbound(ToolCall {
                id: "before".to_string(),
                name: "read".to_string(),
                arguments: json!({"token": placeholder}),
                thought_signature: None,
            })
            .expect("restore source session credential");
        assert_eq!(before_reset.arguments["token"], raw_secret);

        agent.reset_session_scoped_state(crate::plan::PlanMode::Off);

        assert_eq!(agent.plan_state().mode(), crate::plan::PlanMode::Off);
        let after_reset = agent
            .restore_secrets_inbound(ToolCall {
                id: "after".to_string(),
                name: "read".to_string(),
                arguments: json!({"token": placeholder}),
                thought_signature: None,
            })
            .expect("unknown placeholder remains literal");
        assert_eq!(after_reset.arguments["token"], placeholder);
        assert_eq!(agent.mask_secrets_text(raw_secret), raw_secret);
    }

    // === Dialect repair turn (bd-cv653.7.8) ===

    /// Emits a text-embedded tool call on stream 1, plain text on stream 2.
    struct TextCallProvider {
        calls: std::sync::atomic::AtomicUsize,
        model_id: &'static str,
        /// When true, the request after the repaired tool result drops
        /// mid-stream once so retry cleanup can be exercised.
        fail_after_tool_once: bool,
        /// Optional mutation-sensitive assertion for the resumed request.
        expected_retry_thinking: Option<crate::model::ThinkingLevel>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TextCallProvider {
        fn name(&self) -> &str {
            "test-provider"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            self.model_id
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let call_number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call_number >= 2
                && let Some(expected) = self.expected_retry_thinking
            {
                assert_eq!(
                    options.thinking_level,
                    Some(expected),
                    "retry must preserve the original turn's magic-keyword effort"
                );
            }
            let make = |content: Vec<ContentBlock>, reason: StopReason| AssistantMessage {
                content,
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "qwen3-mock".to_string(),
                usage: Usage::default(),
                stop_reason: reason,
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let events: Vec<Result<StreamEvent>> = if call_number == 0 {
                vec![
                    Ok(StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "Checking the file. <tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"fixture.txt\"}}</tool_call>".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: make(
                            vec![ContentBlock::Text(TextContent::new(
                                "Checking the file. <tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"fixture.txt\"}}</tool_call>",
                            ))],
                            StopReason::Stop,
                        ),
                    }),
                ]
            } else if self.fail_after_tool_once && call_number == 1 {
                vec![
                    Ok(StreamEvent::Start {
                        partial: make(Vec::new(), StopReason::Stop),
                    }),
                    Err(Error::api(
                        "SSE error: connection reset by peer (transient connection drop)",
                    )),
                ]
            } else {
                assert!(
                    !context.messages.iter().any(|message| {
                        matches!(
                            message,
                            Message::Assistant(assistant)
                                if matches!(
                                    assistant.stop_reason,
                                    StopReason::Error | StopReason::Aborted
                                )
                        )
                    }),
                    "retry context must not retain the incomplete assistant"
                );
                vec![
                    Ok(StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "The file says hello-fixture.".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: make(
                            vec![ContentBlock::Text(TextContent::new(
                                "The file says hello-fixture.",
                            ))],
                            StopReason::Stop,
                        ),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn dialect_repair_preserves_other_blocks_and_uses_unique_ids() {
        let provider = Arc::new(TextCallProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            model_id: "qwen3-mock",
            fail_after_tool_once: false,
            expected_retry_thinking: None,
        });
        let tools = ToolRegistry::new(&["read"], std::path::Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());
        agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
        let original = AssistantMessage {
            content: vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "reasoning".to_string(),
                    thinking_signature: Some("thinking-sig".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "preface".to_string(),
                    text_signature: Some("preface-sig".to_string()),
                }),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Text(TextContent {
                    text: r#"<tool_call>{"name":"read","arguments":{"path":"fixture.txt"}}</tool_call>"#
                        .to_string(),
                    text_signature: Some("candidate-sig".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "tail".to_string(),
                    text_signature: Some("tail-sig".to_string()),
                }),
            ],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "qwen3-mock".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        let first = agent.maybe_repair_dialect_tool_calls(original.clone());
        let second = agent.maybe_repair_dialect_tool_calls(original);
        assert!(matches!(
            &first.content[0],
            ContentBlock::Thinking(thinking)
                if thinking.thinking_signature.as_deref() == Some("thinking-sig")
        ));
        assert!(matches!(
            &first.content[1],
            ContentBlock::Text(text)
                if text.text == "preface"
                    && text.text_signature.as_deref() == Some("preface-sig")
        ));
        assert!(matches!(&first.content[2], ContentBlock::Image(_)));
        assert!(matches!(
            &first.content[4],
            ContentBlock::Text(text)
                if text.text == "tail" && text.text_signature.as_deref() == Some("tail-sig")
        ));
        let first_id = extract_tool_calls(&first.content)[0].id.clone();
        let second_id = extract_tool_calls(&second.content)[0].id.clone();
        assert_ne!(
            first_id, second_id,
            "repaired call IDs must be session-unique"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dialect_repair_continues_turn_and_executes_tool() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            // CONTROL: Native dialect (no repair path) — a plain-text turn
            // must complete without hitting the iteration cap. If this
            // loops, the test provider is the artifact, not the repair.
            for model_id in ["gpt-4o", "gpt-5.5"] {
                let control = Arc::new(TextCallProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    model_id,
                    fail_after_tool_once: false,
                    expected_retry_thinking: None,
                });
                let control_tools = ToolRegistry::new(&["read"], std::path::Path::new("."), None);
                let mut control_agent = Agent::new(
                    Arc::clone(&control) as Arc<dyn Provider>,
                    control_tools,
                    AgentConfig::default(),
                );
                control_agent.config.max_tool_iterations = 5;
                let session = Arc::new(Mutex::new(Session::in_memory()));
                let mut control_session = AgentSession::new(
                    control_agent,
                    Arc::clone(&session),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tool_starts_for_events = Arc::clone(&tool_starts);
                let control_result = control_session
                    .run_text_with_abort("hello".to_string(), None, move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_starts_for_events.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .await
                    .expect("non-repairable control completes");
                assert_eq!(control_result.stop_reason, StopReason::Stop);
                assert_eq!(control.calls.load(Ordering::SeqCst), 1, "{model_id}");
                assert_eq!(tool_starts.load(Ordering::SeqCst), 0, "{model_id}");
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("control session lock");
                assert!(
                    inner
                        .entries_for_current_path()
                        .iter()
                        .all(|entry| !matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "dialect_repair"
                        )),
                    "{model_id} must not persist repair telemetry"
                );
            }
        });
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: false,
                expected_retry_thinking: None,
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("sessions"),
            ))));
            let mut agent_session =
                AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

            let final_message = agent_session
                .run_text_with_abort("check the fixture".to_string(), None, |_| {})
                .await
                .expect("run completes");

            let texts: Vec<String> = agent_session
                .agent
                .messages()
                .iter()
                .flat_map(|m| match m {
                    crate::model::Message::Assistant(msg) => msg.content.clone(),
                    crate::model::Message::User(u) => match &u.content {
                        crate::model::UserContent::Text(t) => {
                            vec![ContentBlock::Text(TextContent::new(t))]
                        }
                        crate::model::UserContent::Blocks(blocks) => blocks.clone(),
                    },
                    crate::model::Message::ToolResult(r) => r.content.clone(),
                    crate::model::Message::Custom(_) => Vec::new(),
                })
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text),
                    _ => None,
                })
                .collect();
            assert!(
                texts.iter().any(|t| t.contains("hello-fixture")),
                "tool result with fixture content present: {texts:?}"
            );
            assert_eq!(
                final_message.stop_reason,
                StopReason::Stop,
                "turn ends cleanly after the repair"
            );
            // The session layer drained the ledger into a dialect_repair
            // Custom entry at run completion (bd-cv653.7.8).
            let entries = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                let found: Vec<_> = inner
                    .entries_for_current_path()
                    .iter()
                    .filter_map(|e| match e {
                        crate::session::SessionEntry::Custom(c)
                            if c.custom_type == "dialect_repair" =>
                        {
                            Some(c.data.clone())
                        }
                        _ => None,
                    })
                    .collect();
                found
            };
            assert_eq!(entries.len(), 1, "one repair entry in the session");
            assert_eq!(
                entries[0]
                    .as_ref()
                    .and_then(|d| d.get("tool"))
                    .and_then(Value::as_str),
                Some("read")
            );

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved session");
            let persisted_repairs = reopened
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                    )
                })
                .count();
            assert_eq!(persisted_repairs, 1, "repair audit entry survives reopen");
        });
    }

    #[test]
    fn dialect_repair_on_resume_persists_before_exit() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: false,
                expected_retry_thinking: None,
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            let mut durable_session =
                Session::create_with_dir(Some(temp.path().join("resume-sessions")));
            durable_session.append_model_message(Message::User(UserMessage {
                content: UserContent::Text("check the fixture".to_string()),
                timestamp: Utc::now().timestamp_millis(),
            }));
            let session = Arc::new(Mutex::new(durable_session));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );

            let final_message = agent_session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("resume completes");
            assert_eq!(final_message.stop_reason, StopReason::Stop);

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved resume session");
            let persisted_repairs = reopened
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                                && custom.data.as_ref().is_some_and(|data| {
                                    data["tool"] == json!("read")
                                })
                    )
                })
                .count();
            assert_eq!(
                persisted_repairs, 1,
                "resume repair audit entry survives an immediate reopen"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn failed_keyword_repair_turn_keeps_audits_and_reverts_incomplete_tail() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: true,
                expected_retry_thinking: Some(crate::model::ThinkingLevel::High),
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(
                Arc::clone(&provider) as Arc<dyn Provider>,
                tools,
                AgentConfig::default(),
            );
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("retry-sessions"),
            ))));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );
            let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let tool_starts_for_events = Arc::clone(&tool_starts);

            let failed = agent_session
                .run_text_with_abort(
                    "ultrathink check the fixture".to_string(),
                    None,
                    move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_starts_for_events.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                )
                .await
                .expect("mid-stream failure is represented by an assistant message");
            assert_eq!(failed.stop_reason, StopReason::Error);

            {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("failed session lock");
                let entries = inner.entries_for_current_path();
                assert!(matches!(
                    entries.last(),
                    Some(crate::session::SessionEntry::Message(message))
                        if matches!(
                            &message.message,
                            crate::session::SessionMessage::Assistant { message }
                                if message.stop_reason == StopReason::Error
                        )
                ));
                assert_eq!(
                    entries
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "dialect_repair"
                        ))
                        .count(),
                    1
                );
                assert_eq!(
                    entries
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "magic_keyword"
                        ))
                        .count(),
                    1
                );
            }

            assert!(
                agent_session
                    .revert_incomplete_response()
                    .await
                    .expect("revert succeeds"),
                "the failed assistant must remain the removable session tail"
            );
            let resumed = agent_session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("retry resumes after the completed tool cycle");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
            assert_eq!(tool_starts.load(Ordering::SeqCst), 1);

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("resumed session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved retry session");
            let entries = reopened.entries_for_current_path();
            assert!(entries.iter().all(|entry| !matches!(
                entry,
                crate::session::SessionEntry::Message(message)
                    if matches!(
                        &message.message,
                        crate::session::SessionMessage::Assistant { message }
                            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                    )
            )));
            let repair_data = entries.iter().find_map(|entry| match entry {
                crate::session::SessionEntry::Custom(custom)
                    if custom.custom_type == "dialect_repair" =>
                {
                    custom.data.as_ref()
                }
                _ => None,
            });
            assert_eq!(
                repair_data
                    .and_then(|data| data.get("tool"))
                    .and_then(Value::as_str),
                Some("read")
            );
            assert!(
                repair_data
                    .and_then(|data| data.get("strippedBytes"))
                    .and_then(Value::as_u64)
                    .is_some_and(|bytes| bytes > 0),
                "repair telemetry must prove that text was actually stripped"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                    ))
                    .count(),
                1
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "magic_keyword"
                    ))
                    .count(),
                1
            );
        });
    }

    // === Advisor turn review (bd-cv653.3.3) ===

    /// Doer: issues one structured read call, then finishes with text.
    struct ScriptedDoerProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ScriptedDoerProvider {
        fn name(&self) -> &str {
            "doer"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            "doer-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let call_number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let make = |content: Vec<ContentBlock>| AssistantMessage {
                content,
                api: "test-api".to_string(),
                provider: "doer".to_string(),
                model: "doer-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let events: Vec<Result<StreamEvent>> = if call_number == 0 {
                vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: make(vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-1".to_string(),
                        name: "read".to_string(),
                        arguments: json!({"path": "fixture.txt"}),
                        thought_signature: None,
                    })]),
                })]
            } else {
                vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: make(vec![ContentBlock::Text(TextContent::new("all fixed"))]),
                })]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// Advisor: returns a CONCERN verdict.
    struct ScriptedAdvisorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ScriptedAdvisorProvider {
        fn name(&self) -> &str {
            "advisor"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            "advisor-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "CONCERN: the read covers the whole tree".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text(TextContent::new(
                            "CONCERN: the read covers the whole tree",
                        ))],
                        api: "test-api".to_string(),
                        provider: "advisor".to_string(),
                        model: "advisor-model".to_string(),
                        usage: Usage::default(),
                        stop_reason: StopReason::Stop,
                        stop_details: None,
                        error_message: None,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    },
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    struct AdvisorLifecycleDoer {
        replies: StdMutex<VecDeque<Result<AssistantMessage>>>,
        calls: std::sync::atomic::AtomicUsize,
        session: Arc<Mutex<Session>>,
        fail_save_at_completion: StdMutex<Option<PathBuf>>,
        observed_durable_advice: AtomicBool,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for AdvisorLifecycleDoer {
        fn name(&self) -> &str {
            "doer"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "doer-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if context.messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(user)
                        if matches!(&user.content, UserContent::Text(text) if text.starts_with("[ADVISOR:"))
                )
            }) {
                let path = {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let session = self
                        .session
                        .lock(cx.cx())
                        .await
                        .map_err(|error| Error::session(error.to_string()))?;
                    session
                        .path
                        .clone()
                        .ok_or_else(|| Error::session("advisor continuation was not saved"))?
                };
                let reopened = Session::open(path.to_string_lossy().as_ref()).await?;
                let entries = reopened.entries_for_current_path();
                let durable_note = entries.iter().any(|entry| matches!(
                    entry,
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == "advisor_note"
                ));
                let durable_reservation = entries.iter().any(|entry| matches!(
                    entry,
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == ADVISOR_CONTINUATION_ENTRY
                            && custom.data.as_ref()
                                .and_then(|data| data.get("count"))
                                .and_then(Value::as_u64)
                                .is_some_and(|count| count > 0)
                ));
                self.observed_durable_advice
                    .store(durable_note && durable_reservation, Ordering::SeqCst);
            }
            let reply = self
                .replies
                .lock()
                .expect("scripted doer replies") // ubs:ignore[rust.panic.expect] -- Fixture synchronization failure invalidates the regression.
                .pop_front()
                .expect("unexpected doer request")?; // ubs:ignore[rust.panic.expect] -- An extra request is the regression under test.
            let blocked_path = self
                .fail_save_at_completion
                .lock()
                .expect("save failure injection") // ubs:ignore[rust.panic.expect] -- The fixture must retain its one explicit fault.
                .clone();
            if reply.stop_reason == StopReason::Stop
                && let Some(path) = blocked_path
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                    .await
                    .map_err(|error| Error::session(error.to_string()))?;
                session.path = Some(path);
            }
            Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                reason: reply.stop_reason,
                message: reply,
            })])))
        }
    }

    struct AdvisorLifecycleReview {
        calls: std::sync::atomic::AtomicUsize,
        session: Arc<Mutex<Session>>,
        observed_durable_completion: AtomicBool,
        abort_on_call: Option<AbortHandle>,
        verdict: StdMutex<Option<&'static str>>,
        fail_save_on_call: StdMutex<Option<PathBuf>>,
        pending_on_call: AtomicBool,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for AdvisorLifecycleReview {
        fn name(&self) -> &str {
            "advisor"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "advisor-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let path = {
                let cx = crate::agent_cx::AgentCx::for_request();
                let session = self
                    .session
                    .lock(cx.cx())
                    .await
                    .map_err(|error| Error::session(error.to_string()))?;
                session
                    .path
                    .clone()
                    .ok_or_else(|| Error::session("advisor ran before the first save"))?
            };
            let reopened = Session::open(path.to_string_lossy().as_ref()).await?;
            self.observed_durable_completion.store(
                reopened
                    .to_messages_for_current_path()
                    .iter()
                    .any(|message| {
                        matches!(
                            message,
                            Message::Assistant(assistant)
                                if assistant.stop_reason == StopReason::Stop
                                    && assistant_text_content(&assistant.content)
                                        == "durable doer completion"
                        )
                    }),
                Ordering::SeqCst,
            );
            if self.pending_on_call.load(Ordering::SeqCst) {
                futures::future::pending::<()>().await;
            }
            if let Some(handle) = &self.abort_on_call {
                handle.abort();
            }
            let blocked_path = self
                .fail_save_on_call
                .lock()
                .expect("advisor publication fault")
                .clone();
            if let Some(path) = blocked_path {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                    .await
                    .map_err(|error| Error::session(error.to_string()))?;
                session.path = Some(path);
            }
            let verdict = *self.verdict.lock().expect("advisor verdict script");
            if let Some(verdict) = verdict {
                return Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text(TextContent::new(verdict))],
                        api: "test-api".to_string(),
                        provider: "advisor".to_string(),
                        model: "advisor-model".to_string(),
                        stop_reason: StopReason::Stop,
                        ..AssistantMessage::default()
                    },
                })])));
            }
            ScriptedAdvisorProvider.stream(context, options).await
        }
    }

    fn advisor_lifecycle_reply(stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(
                "durable doer completion",
            ))],
            api: "test-api".to_string(),
            provider: "doer".to_string(),
            model: "doer-model".to_string(),
            stop_reason,
            error_message: (stop_reason == StopReason::Error)
                .then(|| "temporary provider failure".to_string()),
            ..AssistantMessage::default()
        }
    }

    fn advisor_lifecycle_tool_call() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "advisor-read".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "fixture.txt"}),
                thought_signature: None,
            })],
            ..advisor_lifecycle_reply(StopReason::ToolUse)
        }
    }

    fn advisor_lifecycle_session(
        root: &Path,
        replies: Vec<Result<AssistantMessage>>,
        abort_on_review: Option<AbortHandle>,
    ) -> (
        AgentSession,
        Arc<AdvisorLifecycleDoer>,
        Arc<AdvisorLifecycleReview>,
    ) {
        std::fs::write(root.join("fixture.txt"), "fixture content").expect("write read fixture"); // ubs:ignore[rust.panic.expect] -- Real read-tool execution establishes a substantial turn.
        let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
            root.join("sessions"),
        ))));
        let doer = Arc::new(AdvisorLifecycleDoer {
            replies: StdMutex::new(replies.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            session: Arc::clone(&session),
            fail_save_at_completion: StdMutex::new(None),
            observed_durable_advice: AtomicBool::new(false),
        });
        let advisor = Arc::new(AdvisorLifecycleReview {
            calls: std::sync::atomic::AtomicUsize::new(0),
            session: Arc::clone(&session),
            observed_durable_completion: AtomicBool::new(false),
            abort_on_call: abort_on_review,
            verdict: StdMutex::new(None),
            fail_save_on_call: StdMutex::new(None),
            pending_on_call: AtomicBool::new(false),
        });
        let mut agent_session = AgentSession::new(
            Agent::new(
                doer.clone(),
                ToolRegistry::new(&["read"], root, None),
                AgentConfig::default(),
            ),
            session,
            true,
            ResolvedCompactionSettings {
                enabled: false,
                ..ResolvedCompactionSettings::default()
            },
        );
        agent_session.advisor = Some(
            crate::advisor::AdvisorRuntime::new(
                advisor.clone(),
                "advisor/advisor-model".to_string(),
            )
            .with_continue_severity(crate::advisor::ContinueSeverity::Blocker),
        );
        (agent_session, doer, advisor)
    }

    #[test]
    fn every_session_entrypoint_reviews_once_after_the_completed_turn_is_durable() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- Each path exercises real AgentSession execution and persistence.
        runtime.block_on(async {
            for entrypoint in ["text", "content", "custom", "continue"] {
                let temp = tempfile::tempdir().expect("tempdir"); // ubs:ignore[rust.panic.expect] -- Isolated durable session and read-tool fixture.
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    ],
                    None,
                );
                if entrypoint == "continue" {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("seed continuation") // ubs:ignore[rust.panic.expect] -- A continuation must start from an authored persisted-path input.
                        .append_model_message(user_message("check the fixture"));
                }
                let ends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let captured_ends = Arc::clone(&ends);
                let on_event = move |event| {
                    if matches!(event, AgentEvent::AgentEnd { .. }) {
                        captured_ends.fetch_add(1, Ordering::SeqCst);
                    }
                };
                let result = match entrypoint {
                    "text" => {
                        session
                            .run_text("check the fixture".to_string(), on_event)
                            .await
                    }
                    "content" => {
                        session
                            .run_with_content(
                                vec![ContentBlock::Text(TextContent::new("check the fixture"))],
                                on_event,
                            )
                            .await
                    }
                    "custom" => {
                        session
                            .run_custom_message_with_abort(
                                Message::Custom(CustomMessage {
                                    content: "check the fixture".to_string(),
                                    custom_type: "extension-task".to_string(),
                                    display: true,
                                    details: None,
                                    timestamp: Utc::now().timestamp_millis(),
                                }),
                                None,
                                on_event,
                            )
                            .await
                    }
                    _ => session.run_continue_with_abort(None, on_event).await,
                }
                .expect("completed session turn"); // ubs:ignore[rust.panic.expect] -- All four actual execution entrypoints must complete.
                assert_eq!(result.stop_reason, StopReason::Stop, "{entrypoint}"); // ubs:ignore[rust.panic.assert-macros] -- The primary result remains unchanged by optional review.
                assert_eq!(doer.calls.load(Ordering::SeqCst), 2, "{entrypoint}"); // ubs:ignore[rust.panic.assert-macros] -- Advice is queued for a later explicit turn.
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 1, "{entrypoint}"); // ubs:ignore[rust.panic.assert-macros] -- Nested wrappers must neither omit nor duplicate review.
                assert!(
                    advisor.observed_durable_completion.load(Ordering::SeqCst),
                    "{entrypoint}"
                ); // ubs:ignore[rust.panic.assert-macros] -- Independent disk reopen at provider admission sees the completed doer reply.
                assert_eq!(ends.load(Ordering::SeqCst), 1, "{entrypoint}"); // ubs:ignore[rust.panic.assert-macros] -- Advisory work adds no extra agent terminal.
                assert_eq!(
                    session.agent.message_queue.steering.len(),
                    1,
                    "{entrypoint}"
                ); // ubs:ignore[rust.panic.assert-macros] -- Exactly one generated concern awaits the next turn.
                assert!(
                    !session.extensions_turn_active.load(Ordering::SeqCst),
                    "{entrypoint}"
                ); // ubs:ignore[rust.panic.assert-macros] -- Completion releases turn ownership on every path.
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn advisor_note_is_visible_before_next_turn_only_after_durable_publication() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for publication_fails in [false, true] {
                let temp = tempfile::tempdir().expect("tempdir");
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    ],
                    None,
                );
                // The fixture's blocker threshold holds this concern for a
                // later authored turn while its card should appear immediately.
                if publication_fails {
                    let blocked_path = temp.path().join("blocked-advisor-note.jsonl");
                    std::fs::create_dir(&blocked_path).expect("block advisor publication");
                    *advisor
                        .fail_save_on_call
                        .lock()
                        .expect("advisor publication fault") = Some(blocked_path);
                }
                let events = Arc::new(StdMutex::new(Vec::new()));
                let captured = Arc::clone(&events);
                let result = session
                    .run_text("check the fixture".to_string(), move |event| {
                        captured.lock().expect("events").push(event);
                    })
                    .await;
                assert_eq!(doer.calls.load(Ordering::SeqCst), 2);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
                assert_eq!(session.agent.message_queue.steering.len(), 1);
                if publication_fails {
                    assert!(result.is_err());
                    assert!(session.ensure_provider_reentry_allowed().is_err());
                    assert!(
                        events
                            .lock()
                            .expect("events")
                            .iter()
                            .all(|event| !matches!(event, AgentEvent::AdvisorNote { .. })),
                        "an unflushed advisor note must never be displayed as published"
                    );
                    continue;
                }
                assert_eq!(
                    result.expect("held note keeps completed answer").stop_reason,
                    StopReason::Stop
                );
                {
                    let events = events.lock().expect("first turn events");
                    let notes = events
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::AdvisorNote { .. }))
                        .collect::<Vec<_>>();
                    assert_eq!(notes.len(), 1);
                    assert!(matches!(
                        notes[0],
                        AgentEvent::AdvisorNote { level, rationale }
                            if level == "CONCERN"
                                && rationale == "the read covers the whole tree"
                    ));
                    let note_index = events
                        .iter()
                        .position(|event| matches!(event, AgentEvent::AdvisorNote { .. }))
                        .expect("immediate note");
                    let end_index = events
                        .iter()
                        .position(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                        .expect("completed terminal");
                    assert!(note_index < end_index);
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                            .count(),
                        1
                    );
                }
                let path = {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("saved note session")
                        .path
                        .clone()
                        .expect("saved note path")
                };
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen published note");
                assert_eq!(
                    reopened
                        .entries_for_current_path()
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "advisor_note"
                        ))
                        .count(),
                    1
                );

                let captured = Arc::clone(&events);
                session
                    .run_text("continue with the next request".to_string(), move |event| {
                        captured.lock().expect("events").push(event);
                    })
                    .await
                    .expect("explicit next turn consumes held context");
                assert_eq!(doer.calls.load(Ordering::SeqCst), 3);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
                assert!(session.agent.message_queue.steering.is_empty());
                assert_eq!(
                    events
                        .lock()
                        .expect("all events")
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::AdvisorNote { .. }))
                        .count(),
                    1,
                    "delivery of already displayed advisor context must not repeat its card"
                );
                assert_eq!(
                    session
                        .agent
                        .messages()
                        .iter()
                        .filter(|message| matches!(
                            message,
                            Message::User(user)
                                if matches!(
                                    &user.content,
                                    UserContent::Text(text)
                                        if text.starts_with("[ADVISOR:CONCERN]")
                                )
                        ))
                        .count(),
                    1
                );
            }
        });
    }

    #[test]
    fn abort_from_immediate_advisor_note_prevents_correction_and_duplicate_delivery() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![
                    Ok(advisor_lifecycle_tool_call()),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                ],
                None,
            );
            session.advisor = Some(crate::advisor::AdvisorRuntime::new(
                advisor.clone(),
                "advisor/advisor-model".to_string(),
            ));
            let (handle, signal) = AbortHandle::new();
            let events = Arc::new(StdMutex::new(Vec::new()));
            let captured = Arc::clone(&events);
            let result = session
                .run_text_with_abort(
                    "check the fixture".to_string(),
                    Some(signal),
                    move |event| {
                        if matches!(event, AgentEvent::AdvisorNote { .. }) {
                            handle.abort();
                        }
                        captured.lock().expect("events").push(event);
                    },
                )
                .await
                .expect("completed answer survives stop from advisor card");
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(doer.calls.load(Ordering::SeqCst), 2);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(session.agent.message_queue.steering.len(), 1);
            assert!(!session.extensions_turn_active.load(Ordering::SeqCst));
            {
                let events = events.lock().expect("stopped turn events");
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::AdvisorNote { .. }))
                        .count(),
                    1
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                        .count(),
                    1
                );
            }
            let captured = Arc::clone(&events);
            session
                .run_text("start a fresh request".to_string(), move |event| {
                    captured.lock().expect("events").push(event);
                })
                .await
                .expect("fresh user turn can consume the held note");
            assert_eq!(doer.calls.load(Ordering::SeqCst), 3);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
            assert!(session.agent.message_queue.steering.is_empty());
            assert_eq!(
                events
                    .lock()
                    .expect("all events")
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AdvisorNote { .. }))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn default_advisor_concern_continues_before_the_only_terminal_event() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut corrected = advisor_lifecycle_reply(StopReason::Stop);
            corrected.content = vec![ContentBlock::Text(TextContent::new(
                "corrected after advisor feedback",
            ))];
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![
                    Ok(advisor_lifecycle_tool_call()),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    Ok(corrected),
                ],
                None,
            );
            // Exercise the production default rather than an explicit concern
            // override; the legacy lifecycle fixture intentionally uses blocker.
            session.advisor = Some(crate::advisor::AdvisorRuntime::new(
                advisor.clone(),
                "advisor/advisor-model".to_string(),
            ));
            session.agent.config.max_time = Some(Duration::from_secs(600));
            let events = Arc::new(StdMutex::new(Vec::new()));
            let captured = Arc::clone(&events);
            let result = session
                .run_text("check the fixture".to_string(), move |event| {
                    captured.lock().expect("events").push(event);
                })
                .await
                .expect("reviewed completion");
            assert_eq!(
                assistant_text_content(&result.content),
                "corrected after advisor feedback"
            );
            assert_eq!(doer.calls.load(Ordering::SeqCst), 3);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
            assert!(advisor.observed_durable_completion.load(Ordering::SeqCst));
            assert!(doer.observed_durable_advice.load(Ordering::SeqCst));
            assert!(session.agent.message_queue.steering.is_empty());
            assert_eq!(session.agent.config.max_time, Some(Duration::from_secs(600)));
            assert!(!session.extensions_turn_active.load(Ordering::SeqCst));

            let events = events.lock().expect("events");
            assert_eq!(
                events.iter().filter(|event| matches!(event, AgentEvent::AgentEnd { .. })).count(),
                1
            );
            assert_eq!(
                events.iter().filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. })).count(),
                1
            );
            let Some(AgentEvent::AgentEnd { messages, error, .. }) = events.last() else {
                panic!("the complete reviewed turn must end with one terminal");
            };
            assert!(error.is_none());
            assert!(matches!(
                messages.last(),
                Some(Message::Assistant(assistant))
                    if assistant_text_content(&assistant.content) == "corrected after advisor feedback"
            ));
            assert_eq!(
                messages.iter().filter(|message| matches!(
                    message, Message::User(user)
                        if matches!(&user.content, UserContent::Text(text) if text == "check the fixture")
                )).count(),
                1,
                "automatic continuation must not resubmit the authored input"
            );
            drop(events);
        });
    }

    #[test]
    fn dropping_advisor_review_releases_the_completed_terminal_once() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(Box::pin(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![
                    Ok(advisor_lifecycle_tool_call()),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                ],
                None,
            );
            advisor.pending_on_call.store(true, Ordering::SeqCst);
            session.agent.config.max_time = Some(Duration::from_secs(600));
            session.agent.set_system_prompt(Some("base-system".to_string()));
            let active = Arc::clone(&session.extensions_turn_active);
            let streaming = Arc::clone(&session.extensions_is_streaming);
            let events = Arc::new(StdMutex::new(Vec::new()));
            let captured = Arc::clone(&events);
            let mut turn = Box::pin(session.run_text(
                "check the fixture".to_string(),
                move |event| captured.lock().expect("events").push(event),
            ));
            let reached_review = futures::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(turn.as_mut(), cx).is_pending());
                if advisor.observed_durable_completion.load(Ordering::SeqCst) {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            });
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                reached_review,
            )
            .await
            .expect("advisor must hold after durable native completion");
            assert_eq!(doer.calls.load(Ordering::SeqCst), 2);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
            assert!(active.load(Ordering::SeqCst));
            assert!(events.lock().expect("events").iter().all(|event| {
                !matches!(event, AgentEvent::AgentEnd { .. })
            }));

            drop(turn);

            assert!(!active.load(Ordering::SeqCst));
            assert!(!streaming.load(Ordering::SeqCst));
            assert_eq!(session.agent.system_prompt(), Some("base-system"));
            assert_eq!(session.agent.config.max_time, Some(Duration::from_secs(600)));
            assert!(session.agent.message_queue.steering.is_empty());
            let events = events.lock().expect("events after drop");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                    .count(),
                1,
            );
            assert!(matches!(
                events.last(),
                Some(AgentEvent::AgentEnd { error: Some(error), .. }) if error == "Aborted"
            ));
            drop(events);
        }));
    }

    #[test]
    fn advisor_blocker_continuation_cap_survives_fresh_session_runtime() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut reviewed = advisor_lifecycle_reply(StopReason::Stop);
            reviewed.content = vec![ContentBlock::Text(TextContent::new(
                "Reviewed the requested changes and recorded the result. ".repeat(10),
            ))];
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![
                    Ok(advisor_lifecycle_tool_call()),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    Ok(reviewed.clone()),
                    Ok(reviewed.clone()),
                    Ok(reviewed),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                ],
                None,
            );
            *advisor.verdict.lock().expect("blocker script") =
                Some("BLOCKER: the completed change still needs a correction");
            session
                .run_text("check the fixture".to_string(), |_| {})
                .await
                .expect("bounded reviewed turn");
            assert_eq!(doer.calls.load(Ordering::SeqCst), 4);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 3);
            assert_eq!(session.agent.message_queue.steering.len(), 1);
            assert!(doer.observed_durable_advice.load(Ordering::SeqCst));

            let path = {
                let cx = crate::agent_cx::AgentCx::for_request();
                session.session.lock(cx.cx()).await.expect("session").path.clone().expect("path")
            };
            let reopened = Session::open(path.to_string_lossy().as_ref()).await.expect("reopen");
            let reservations = reopened.entries_for_current_path().into_iter().filter_map(|entry| {
                match entry {
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == ADVISOR_CONTINUATION_ENTRY =>
                    {
                        custom.data.as_ref().and_then(|data| data.get("count")).and_then(Value::as_u64)
                    }
                    _ => None,
                }
            }).collect::<Vec<_>>();
            assert_eq!(reservations, vec![0, 1, 2]);

            // Install the independently reopened journal in a completely fresh
            // Agent and AdvisorRuntime. Neither their queues nor emission guard
            // can supply the saved continuation limit.
            let shared_session = Arc::clone(&session.session);
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                *shared_session.lock(cx.cx()).await.expect("replace with reopened") = reopened;
            }
            let mut resumed = AgentSession::new(
                Agent::new(
                    doer.clone(),
                    ToolRegistry::new(&["read"], temp.path(), None),
                    AgentConfig::default(),
                ),
                shared_session,
                true,
                ResolvedCompactionSettings {
                    enabled: false,
                    ..ResolvedCompactionSettings::default()
                },
            );
            resumed.advisor = Some(crate::advisor::AdvisorRuntime::new(
                advisor.clone(),
                "advisor/advisor-model".to_string(),
            ));
            resumed.run_continue_with_abort(None, |_| {}).await.expect("explicit resume");
            assert_eq!(doer.calls.load(Ordering::SeqCst), 5);
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 4);
            assert_eq!(resumed.agent.message_queue.steering.len(), 1);

            resumed.run_text("a new task".to_string(), |_| {}).await.expect("new authored input");
            assert_eq!(doer.calls.load(Ordering::SeqCst), 6);
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = resumed.session.lock(cx.cx()).await.expect("new turn state");
            let count = inner.entries_for_current_path().into_iter().rev().find_map(|entry| {
                match entry {
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == ADVISOR_CONTINUATION_ENTRY =>
                    {
                        custom.data.as_ref().and_then(|data| data.get("count")).and_then(Value::as_u64)
                    }
                    _ => None,
                }
            });
            assert_eq!(count, Some(0), "new authored input renews the bounded budget");
        });
    }

    fn advisor_reservation_counts(session: &Session) -> Vec<u64> {
        session
            .entries_for_current_path()
            .into_iter()
            .filter_map(|entry| match entry {
                crate::session::SessionEntry::Custom(custom)
                    if custom.custom_type == ADVISOR_CONTINUATION_ENTRY =>
                {
                    custom
                        .data
                        .as_ref()
                        .and_then(|data| data.get("count"))
                        .and_then(Value::as_u64)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn queued_authored_inputs_renew_advisor_budget_and_generated_inputs_do_not() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for (authored, follow_up) in [(true, false), (true, true), (false, false), (false, true)] {
                let temp = tempfile::tempdir().expect("tempdir");
                let mut reviewed = advisor_lifecycle_reply(StopReason::Stop);
                reviewed.content = vec![ContentBlock::Text(TextContent::new(
                    "Reviewed the queued task and recorded the requested correction. ".repeat(10),
                ))];
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                        Ok(reviewed.clone()),
                        Ok(reviewed.clone()),
                        Ok(reviewed),
                    ],
                    None,
                );
                let path = {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let mut inner = session.session.lock(cx.cx()).await.expect("seed session");
                    inner.append_model_message(user_message("previous authored task"));
                    inner.append_custom_entry(
                        ADVISOR_CONTINUATION_ENTRY.to_string(),
                        Some(json!({"count": MAX_ADVISOR_CONTINUATIONS})),
                    );
                    inner.save().await.expect("persist exhausted previous budget");
                    inner.path.clone().expect("saved path")
                };
                *advisor.verdict.lock().expect("blocker script") =
                    Some("BLOCKER: the queued task needs another correction");
                let message = user_message("check the fixture for the queued task");
                let delivery = if authored {
                    // An empty keyword source still proves authorship. Budget
                    // renewal must not depend on finding a magic keyword.
                    QueuedAgentMessage::authored(message, "")
                } else {
                    QueuedAgentMessage::generated(message)
                };
                if follow_up {
                    session.agent.message_queue.push_follow_up(delivery);
                } else {
                    session.agent.message_queue.push_steering(delivery);
                }
                session
                    .run_continue_with_follow_up_with_abort(follow_up, None, || true, |_| {})
                    .await
                    .expect("queued turn");
                let expected_calls = if authored { 4 } else { 2 };
                assert_eq!(doer.calls.load(Ordering::SeqCst), expected_calls);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), if authored { 3 } else { 1 });
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen queued turn");
                assert_eq!(
                    advisor_reservation_counts(&reopened),
                    if authored { vec![2, 0, 1, 2] } else { vec![2] },
                    "authored={authored}, follow_up={follow_up}"
                );
                let shared_session = Arc::clone(&session.session);
                {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    *shared_session.lock(cx.cx()).await.expect("replace session") = reopened;
                }
                let mut resumed = AgentSession::new(
                    Agent::new(
                        doer.clone(),
                        ToolRegistry::new(&["read"], temp.path(), None),
                        AgentConfig::default(),
                    ),
                    shared_session,
                    true,
                    ResolvedCompactionSettings {
                        enabled: false,
                        ..ResolvedCompactionSettings::default()
                    },
                );
                resumed.advisor = Some(crate::advisor::AdvisorRuntime::new(
                    advisor.clone(),
                    "advisor/advisor-model".to_string(),
                ));
                resumed
                    .run_continue_with_abort(None, |_| {})
                    .await
                    .expect("fresh runtime resumes without renewing exhausted budget");
                assert_eq!(doer.calls.load(Ordering::SeqCst), expected_calls + 1);
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen explicit retry");
                assert_eq!(
                    advisor_reservation_counts(&reopened),
                    if authored { vec![2, 0, 1, 2] } else { vec![2] }
                );
            }
        });
    }

    #[test]
    fn queued_authored_budget_boundary_survives_failed_and_aborted_attempts() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for stop_reason in [StopReason::Error, StopReason::Aborted] {
                let temp = tempfile::tempdir().expect("tempdir");
                let mut reviewed = advisor_lifecycle_reply(StopReason::Stop);
                reviewed.content = vec![ContentBlock::Text(TextContent::new(
                    "Recovered the queued task while preserving its completed tool work. ".repeat(10),
                ))];
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(stop_reason)),
                        Ok(reviewed.clone()),
                        Ok(reviewed.clone()),
                        Ok(reviewed),
                    ],
                    None,
                );
                let path = {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let mut inner = session.session.lock(cx.cx()).await.expect("seed session");
                    inner.append_model_message(user_message("previous authored task"));
                    inner.append_custom_entry(
                        ADVISOR_CONTINUATION_ENTRY.to_string(),
                        Some(json!({"count": MAX_ADVISOR_CONTINUATIONS})),
                    );
                    inner.save().await.expect("persist exhausted budget");
                    inner.path.clone().expect("saved path")
                };
                *advisor.verdict.lock().expect("blocker script") =
                    Some("BLOCKER: the recovered task needs another correction");
                session.agent.queue_steering(user_message("check the fixture again"));
                let failed = session
                    .run_continue_with_abort(None, |_| {})
                    .await
                    .expect("failed provider terminal");
                assert_eq!(failed.stop_reason, stop_reason);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 0);
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen incomplete queued turn");
                assert_eq!(advisor_reservation_counts(&reopened), vec![2, 0]);
                assert!(session.revert_incomplete_response().await.expect("remove failed tail"));
                session
                    .run_continue_with_abort(None, |_| {})
                    .await
                    .expect("retry queued turn");
                assert_eq!(doer.calls.load(Ordering::SeqCst), 5);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 3);
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen recovered queued turn");
                assert_eq!(advisor_reservation_counts(&reopened), vec![2, 0, 1, 2]);
                let completed_reads = reopened
                    .to_messages_for_current_path()
                    .into_iter()
                    .filter(|message| matches!(message, Message::ToolResult(result) if result.tool_name == "read"))
                    .count();
                assert_eq!(completed_reads, 1, "retry must not replay the completed read");
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn terminal_arriving_input_renews_advisor_budget_only_when_authored() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for terminal in ["provider_error", "assistant_error", "aborted"] {
                for authored in [true, false] {
                    let temp = tempfile::tempdir().expect("tempdir");
                    let mut reviewed = advisor_lifecycle_reply(StopReason::Stop);
                    reviewed.content = vec![ContentBlock::Text(TextContent::new(
                        "Recovered the late queued task while retaining its completed read. ".repeat(10),
                    ))];
                    let failed_reply = match terminal {
                        "provider_error" => Err(Error::api("injected provider stream failure")),
                        "assistant_error" => Ok(advisor_lifecycle_reply(StopReason::Error)),
                        _ => Ok(advisor_lifecycle_reply(StopReason::Aborted)),
                    };
                    let (mut session, doer, advisor) = advisor_lifecycle_session(
                        temp.path(),
                        vec![
                            Ok(advisor_lifecycle_tool_call()),
                            failed_reply,
                            Ok(reviewed.clone()),
                            Ok(reviewed.clone()),
                            Ok(reviewed),
                        ],
                        None,
                    );
                    let path = {
                        let cx = crate::agent_cx::AgentCx::for_request();
                        let mut inner = session.session.lock(cx.cx()).await.expect("seed session");
                        inner.append_model_message(user_message("previous authored task"));
                        inner.append_custom_entry(
                            ADVISOR_CONTINUATION_ENTRY.to_string(),
                            Some(json!({"count": MAX_ADVISOR_CONTINUATIONS})),
                        );
                        inner.save().await.expect("persist exhausted budget");
                        inner.path.clone().expect("saved path")
                    };
                    *advisor.verdict.lock().expect("blocker script") =
                        Some("BLOCKER: the late queued task needs another correction");
                    let fetched = Arc::new(AtomicBool::new(false));
                    let fetcher: MessageFetcher = {
                        let doer = Arc::clone(&doer);
                        let fetched = Arc::clone(&fetched);
                        Arc::new(move || {
                            let doer = Arc::clone(&doer);
                            let fetched = Arc::clone(&fetched);
                            Box::pin(async move {
                                // The first request performs the read. This
                                // input arrives only after the next request has
                                // started, so a terminal drain must persist it.
                                if doer.calls.load(Ordering::SeqCst) < 2
                                    || fetched.swap(true, Ordering::SeqCst)
                                {
                                    return Vec::new();
                                }
                                let message = user_message("late queued task");
                                vec![if authored {
                                    QueuedAgentMessage::authored(message, "")
                                } else {
                                    QueuedAgentMessage::generated(message)
                                }]
                            })
                        })
                    };
                    session.agent.register_message_fetchers(Some(fetcher), None);
                    let failed = session.run_continue_with_abort(None, |_| {}).await;
                    if terminal == "provider_error" {
                        assert!(failed.is_err(), "provider failure must remain an error");
                    } else {
                        assert_eq!(
                            failed.expect("incomplete assistant").stop_reason,
                            if terminal == "assistant_error" {
                                StopReason::Error
                            } else {
                                StopReason::Aborted
                            }
                        );
                    }
                    assert!(fetched.load(Ordering::SeqCst));
                    assert_eq!(doer.calls.load(Ordering::SeqCst), 2);
                    assert_eq!(advisor.calls.load(Ordering::SeqCst), 0);
                    let reopened = Session::open(path.to_string_lossy().as_ref())
                        .await
                        .expect("reopen terminal delivery");
                    assert_eq!(
                        advisor_reservation_counts(&reopened),
                        if authored { vec![2, 0] } else { vec![2] },
                        "terminal={terminal}, authored={authored}"
                    );
                    session
                        .revert_incomplete_response()
                        .await
                        .expect("remove incomplete tail when present");
                    session
                        .run_continue_with_abort(None, |_| {})
                        .await
                        .expect("resume late queued task");
                    assert_eq!(doer.calls.load(Ordering::SeqCst), if authored { 5 } else { 3 });
                    assert_eq!(advisor.calls.load(Ordering::SeqCst), if authored { 3 } else { 1 });
                    let reopened = Session::open(path.to_string_lossy().as_ref())
                        .await
                        .expect("reopen recovered terminal delivery");
                    assert_eq!(
                        advisor_reservation_counts(&reopened),
                        if authored { vec![2, 0, 1, 2] } else { vec![2] }
                    );
                    let messages = reopened.to_messages_for_current_path();
                    assert_eq!(
                        messages.iter().filter(|message| matches!(
                            message,
                            Message::ToolResult(result) if result.tool_name == "read"
                        )).count(),
                        1,
                        "the completed read must not replay"
                    );
                    assert_eq!(
                        messages.iter().filter(|message| matches!(
                            message,
                            Message::User(user)
                                if matches!(&user.content, UserContent::Text(text) if text == "late queued task")
                        )).count(),
                        1,
                        "the terminal input must remain durable exactly once"
                    );
                }
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one end-to-end advisor fixture with its full assertions
    fn extension_input_does_not_renew_advisor_continuation_budget() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for entrypoint in ["extension_text", "extension_custom", "extension_content", "custom"] {
                let temp = tempfile::tempdir().expect("tempdir");
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    ],
                    None,
                );
                let previous_source = session.input_source;
                {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    let mut inner = session.session.lock(cx.cx()).await.expect("seed cap");
                    inner.append_model_message(user_message("original authored task"));
                    inner.append_custom_entry(
                        ADVISOR_CONTINUATION_ENTRY.to_string(),
                        Some(json!({"count": MAX_ADVISOR_CONTINUATIONS})),
                    );
                }
                *advisor.verdict.lock().expect("blocker script") =
                    Some("BLOCKER: the completed change still needs a correction");
                let custom = Message::Custom(CustomMessage {
                    content: "check the fixture".to_string(),
                    custom_type: "extension-task".to_string(),
                    display: false,
                    details: None,
                    timestamp: 0,
                });
                match entrypoint {
                    "extension_text" | "extension_custom" => {
                        let action = if entrypoint == "extension_text" {
                            PendingIdleAction::UserText("check the fixture".to_string())
                        } else {
                            PendingIdleAction::CustomMessage(custom)
                        };
                        session
                            .extensions_pending_idle_actions
                            .lock()
                            .expect("pending extension action")
                            .push_back(action);
                        session
                            .run_pending_idle_actions_with_abort(None, Arc::new(|_| {}))
                            .await
                            .expect("native extension dispatch");
                    }
                    "extension_content" => {
                        let source_scope =
                            SessionInputSourceGuard::new(&mut session, InputSource::Extension);
                        source_scope
                            .session
                            .run_with_content(
                                vec![ContentBlock::Text(TextContent::new("check the fixture"))],
                                |_| {},
                            )
                            .await
                            .expect("extension content");
                    }
                    _ => {
                        session
                            .run_custom_message_with_abort(custom, None, |_| {})
                            .await
                            .expect("custom input");
                    }
                }
                assert_eq!(session.input_source, previous_source, "{entrypoint}");
                assert_eq!(doer.calls.load(Ordering::SeqCst), 2, "{entrypoint}");
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 1, "{entrypoint}");
                assert_eq!(session.agent.message_queue.steering.len(), 1, "{entrypoint}");

                let path = {
                    let cx = crate::agent_cx::AgentCx::for_request();
                    session
                        .session
                        .lock(cx.cx())
                        .await
                        .expect("inspect session")
                        .path
                        .clone()
                        .expect("saved path")
                };
                let reopened = Session::open(path.to_string_lossy().as_ref())
                    .await
                    .expect("reopen generated turn");
                let reservations = reopened
                    .entries_for_current_path()
                    .into_iter()
                    .filter_map(|entry| match entry {
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == ADVISOR_CONTINUATION_ENTRY =>
                        {
                            custom
                                .data
                                .as_ref()
                                .and_then(|data| data.get("count"))
                                .and_then(Value::as_u64)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(reservations, vec![MAX_ADVISOR_CONTINUATIONS], "{entrypoint}");
            }
        });
    }

    #[test]
    fn advisor_respects_paused_automatic_queue_lanes() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for lanes in [(false, true), (true, false)] {
                let temp = tempfile::tempdir().expect("tempdir");
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(StopReason::Stop)),
                    ],
                    None,
                );
                session.advisor = Some(crate::advisor::AdvisorRuntime::new(
                    advisor.clone(),
                    "advisor/advisor-model".to_string(),
                ));
                session.agent.set_automatic_queue_dispatch(lanes.0, lanes.1);
                session.run_text("check the fixture".to_string(), |_| {}).await.expect("paused lanes");
                assert_eq!(doer.calls.load(Ordering::SeqCst), 2);
                assert_eq!(advisor.calls.load(Ordering::SeqCst), 1);
                assert_eq!(session.agent.message_queue.steering.len(), 1);
            }
        });
    }

    #[test]
    fn advisor_reviews_recovered_tool_work_once_without_reexecuting_it() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- Exercise the same continuation path used by provider retry.
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir"); // ubs:ignore[rust.panic.expect] -- Durable retry fixture.
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![
                    Ok(advisor_lifecycle_tool_call()),
                    Ok(advisor_lifecycle_reply(StopReason::Error)),
                    Ok(advisor_lifecycle_reply(StopReason::Stop)),
                ],
                None,
            );
            let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed_tools = Arc::clone(&tool_starts);
            let failed = session
                .run_text("check the fixture".to_string(), move |event| {
                    if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                        observed_tools.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .await
                .expect("provider terminal is returned"); // ubs:ignore[rust.panic.expect] -- The scripted failure is a completed Error terminal.
            assert_eq!(failed.stop_reason, StopReason::Error); // ubs:ignore[rust.panic.assert-macros] -- Review must be withheld for the failed first attempt.
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Substantial but failed work never starts review.
            assert!(
                session
                    .revert_incomplete_response()
                    .await
                    .expect("revert failed tail")
            ); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect] -- Retry restores the real incomplete assistant tail.
            let observed_tools = Arc::clone(&tool_starts);
            let recovered = session
                .run_continue_with_abort(None, move |event| {
                    if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                        observed_tools.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .await
                .expect("retry completion"); // ubs:ignore[rust.panic.expect] -- The successful retry must reach the shared review boundary.
            assert_eq!(recovered.stop_reason, StopReason::Stop); // ubs:ignore[rust.panic.assert-macros] -- Recovery returns the durable final reply.
            assert_eq!(doer.calls.load(Ordering::SeqCst), 3); // ubs:ignore[rust.panic.assert-macros] -- One tool request, one failed request, and one resumed request.
            assert_eq!(tool_starts.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Completed tool effects are never replayed for review.
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Only the recovered completion is reviewed.
            assert!(advisor.observed_durable_completion.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- Recovered reply is durable before the advisor sees it.
            assert_eq!(session.agent.message_queue.steering.len(), 1); // ubs:ignore[rust.panic.assert-macros] -- Recovery publishes one concern.
        });
    }

    #[test]
    fn durable_time_cap_skips_nontrivial_advice_until_explicit_continuation() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- Exercise the actual zero-budget core boundary.
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir"); // ubs:ignore[rust.panic.expect] -- The capped conversation must survive an independent reopen.
            let (mut session, doer, advisor) = advisor_lifecycle_session(
                temp.path(),
                vec![Ok(advisor_lifecycle_reply(StopReason::Stop))],
                None,
            );
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut inner = session.session.lock(cx.cx()).await.expect("seed tool work"); // ubs:ignore[rust.panic.expect] -- A completed tool cycle makes this digest nontrivial even at a zero cap.
                inner.append_model_message(user_message("check the fixture"));
                inner.append_model_message(Message::assistant(advisor_lifecycle_tool_call()));
                inner.append_model_message(Message::tool_result(ToolResultMessage {
                    tool_call_id: "advisor-read".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ContentBlock::Text(TextContent::new("fixture content"))],
                    details: None,
                    is_error: false,
                    timestamp: Utc::now().timestamp_millis(),
                }));
            }
            session.agent.config.max_time = Some(Duration::ZERO);
            let capped = session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("durable capped turn"); // ubs:ignore[rust.panic.expect] -- The real cap is a successful bounded terminal.
            assert!(time_cap_marker(&capped).is_some()); // ubs:ignore[rust.panic.assert-macros] -- The test must reach the exact synthetic time-cap path.
            assert!(!crate::advisor::build_digest(session.agent.messages()).is_trivial()); // ubs:ignore[rust.panic.assert-macros] -- Zero calls must result from the cap gate rather than trivial-digest suppression.
            assert_eq!(doer.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Exhausted budget admits no primary provider request.
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Exhausted budget also admits no optional review.
            assert!(session.agent.message_queue.steering.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- A pause cannot create new generated work.
            let path = {
                let cx = crate::agent_cx::AgentCx::for_request();
                session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("saved cap path") // ubs:ignore[rust.panic.expect] -- Capture the authoritative file after completed persistence.
                    .path
                    .clone()
                    .expect("durable session path") // ubs:ignore[rust.panic.expect] -- Saving the cap must materialize a session file.
            };
            let reopened = Session::open(path.to_string_lossy().as_ref())
                .await
                .expect("reopen capped session"); // ubs:ignore[rust.panic.expect] -- Assert durable output rather than only the in-memory result.
            assert!(reopened.to_messages_for_current_path().iter().any(
                |message| matches!( // ubs:ignore[rust.panic.assert-macros] -- The exact bounded terminal must survive reopen.
                    message, Message::Assistant(assistant) if time_cap_marker(assistant).is_some()
                )
            ));
            session.agent.config.max_time = None;
            session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("explicit continuation"); // ubs:ignore[rust.panic.expect] -- A subsequent authorized turn may complete the paused work.
            assert_eq!(doer.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Only explicit continuation enters the primary provider.
            assert_eq!(advisor.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Review resumes after a fresh durable completion.
            assert!(advisor.observed_durable_completion.load(Ordering::SeqCst)); // ubs:ignore[rust.panic.assert-macros] -- The new completed reply is saved before review.
        });
    }

    #[test]
    fn advisor_omits_aborted_unsaved_and_cancelled_completions() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build"); // ubs:ignore[rust.panic.expect] -- All cases use the real session completion boundary.
        runtime.block_on(async {
            for case in [
                "aborted",
                "save_failed",
                "late_abort",
                "advisor_abort",
                "advisor_save_failed",
            ] {
                let temp = tempfile::tempdir().expect("tempdir"); // ubs:ignore[rust.panic.expect] -- Each fault owns a separate durable session.
                let (handle, signal) = AbortHandle::new();
                let (mut session, doer, advisor) = advisor_lifecycle_session(
                    temp.path(),
                    vec![
                        Ok(advisor_lifecycle_tool_call()),
                        Ok(advisor_lifecycle_reply(if case == "aborted" {
                            StopReason::Aborted
                        } else {
                            StopReason::Stop
                        })),
                    ],
                    (case == "advisor_abort").then(|| handle.clone()),
                );
                if case == "save_failed" {
                    let blocked_path = temp.path().join("blocked.jsonl");
                    std::fs::create_dir(&blocked_path).expect("block terminal save"); // ubs:ignore[rust.panic.expect] -- A directory at the final file path creates a real save failure.
                    *doer
                        .fail_save_at_completion
                        .lock()
                        .expect("inject save failure") // ubs:ignore[rust.panic.expect] -- Install the failure after the initial authored input is saved.
                        = Some(blocked_path);
                }
                if case == "advisor_save_failed" {
                    let blocked_path = temp.path().join("advisor-blocked.jsonl");
                    std::fs::create_dir(&blocked_path).expect("block advisor note save");
                    *advisor.fail_save_on_call.lock().expect("publication fault") =
                        Some(blocked_path);
                    session.advisor = Some(crate::advisor::AdvisorRuntime::new(
                        advisor.clone(),
                        "advisor/advisor-model".to_string(),
                    ));
                }
                let result = session
                    .run_text_with_abort(
                        "check the fixture".to_string(),
                        Some(signal),
                        move |event| {
                            if case == "late_abort"
                                && matches!(
                                    event,
                                    AgentEvent::TurnEnd {
                                        message: Message::Assistant(assistant),
                                        ..
                                    } if assistant.stop_reason == StopReason::Stop
                                )
                            {
                                handle.abort();
                            }
                        },
                    )
                    .await;
                if matches!(case, "save_failed" | "advisor_save_failed") {
                    assert!(result.is_err()); // ubs:ignore[rust.panic.assert-macros] -- A failed durable completion cannot report success.
                    assert!(session.ensure_provider_reentry_allowed().is_err()); // ubs:ignore[rust.panic.assert-macros] -- The existing persistence fence remains installed.
                } else {
                    let reply = result.expect("primary terminal remains available"); // ubs:ignore[rust.panic.expect] -- Optional review cancellation does not rewrite a completed main turn.
                    assert_eq!(reply.stop_reason, if case == "aborted" { StopReason::Aborted } else { StopReason::Stop }); // ubs:ignore[rust.panic.assert-macros] -- Main-turn outcomes retain their original meaning.
                }
                assert_eq!(doer.calls.load(Ordering::SeqCst), 2, "{case}"); // ubs:ignore[rust.panic.assert-macros] -- Every fault occurs after substantive tool work.
                assert_eq!(advisor.calls.load(Ordering::SeqCst), usize::from(matches!(case, "advisor_abort" | "advisor_save_failed")), "{case}"); // ubs:ignore[rust.panic.assert-macros] -- Only the deliberate in-review cancellation case enters the advisor.
                assert_eq!(
                    session.agent.message_queue.steering.len(),
                    usize::from(case == "advisor_save_failed"),
                    "{case}"
                ); // ubs:ignore[rust.panic.assert-macros] -- A failed note save retains its live note behind the provider-admission fence.
                let cx = crate::agent_cx::AgentCx::for_request();
                let inner = session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("inspect advisor trail"); // ubs:ignore[rust.panic.expect] -- Publication is checked through the authoritative session path.
                let note_count = inner.entries_for_current_path().iter().filter(|entry| matches!(
                    entry, crate::session::SessionEntry::Custom(custom) if custom.custom_type == "advisor_note"
                )).count();
                assert_eq!(note_count, usize::from(case == "advisor_save_failed"), "{case}"); // ubs:ignore[rust.panic.assert-macros] -- Only the explicit publication fault has an unflushed audit note.
            }
        });
    }

    #[test]
    fn advisor_review_injects_concern_into_steering_queue() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "x").expect("fixture");
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let agent = Agent::new(
                Arc::new(ScriptedDoerProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                }),
                tools,
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
            agent_session.advisor = Some(
                crate::advisor::AdvisorRuntime::new(
                    Arc::new(ScriptedAdvisorProvider),
                    "advisor/advisor-model".to_string(),
                )
                .with_continue_severity(crate::advisor::ContinueSeverity::Blocker),
            );

            let _ = agent_session
                .run_text_with_abort("check the fixture".to_string(), None, |_| {})
                .await
                .expect("turn 1 completes");

            // Turn 2 delivers the queued advisor note into the session
            // context (the next-turn injection is the acceptance behavior).
            let _ = agent_session
                .run_text_with_abort("continue".to_string(), None, |_| {})
                .await
                .expect("turn 2 completes");

            let steered = agent_session
                .agent
                .messages()
                .iter()
                .filter_map(|m| match m {
                    crate::model::Message::User(u) => match &u.content {
                        crate::model::UserContent::Text(t) => Some(t.clone()),
                        crate::model::UserContent::Blocks(_) => None,
                    },
                    _ => None,
                })
                .any(|text| text.contains("ADVISOR:CONCERN"));
            assert!(
                steered,
                "advisor concern must be delivered into the next turn"
            );
            let has_entry = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                inner.entries_for_current_path().iter().any(|e| {
                    matches!(
                        e,
                        crate::session::SessionEntry::Custom(c) if c.custom_type == "advisor_note"
                    )
                })
            };
            assert!(has_entry, "advisor_note session entry recorded");
        });
    }

    #[test]
    fn advisor_hook_blocks_configured_and_remembered_secrets_before_provider_admission() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let advisor = Arc::new(ScriptedDoerProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut agent = Agent::new(
                Arc::new(SilentProvider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let known = "rememberedCredentialValue123456";
            agent
                .secrets_transform_outbound_text(&format!("API_KEY={known}"))
                .unwrap();
            let mut session = AgentSession::new(
                agent,
                Arc::new(Mutex::new(Session::in_memory())),
                false,
                ResolvedCompactionSettings::default(),
            );
            session.advisor = Some(
                crate::advisor::AdvisorRuntime::new(advisor.clone(), "privacy-test".to_string())
                    .with_secrets_settings(Some(&crate::secrets::SecretsSettings {
                        mode: Some("block".to_string()),
                        extra_patterns: Some(vec![r"ACME-\d{6}".to_string()]),
                    })),
            );
            for command in [
                format!("{} ACME-123456", "x".repeat(300)),
                "API_KEY=r4nd0mCredentialValue123456".to_string(),
                format!("echo {known}"),
            ] {
                session
                    .agent
                    .replace_messages(vec![Message::Assistant(Arc::new(AssistantMessage {
                        content: vec![ContentBlock::ToolCall(ToolCall {
                            id: "privacy-call".to_string(),
                            name: "bash".to_string(),
                            arguments: json!({"command": command}),
                            thought_signature: None,
                        })],
                        ..Default::default()
                    }))]);
                let original = serde_json::to_value(session.agent.messages()).unwrap();
                let turn = AdvisorTurn::new(&session, Arc::new(|_| {}));
                session.maybe_advise_turn(None, &turn, false).await.unwrap();
                assert_eq!(advisor.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert_eq!(
                    serde_json::to_value(session.agent.messages()).unwrap(),
                    original
                );
                assert!(!session.advisor.as_ref().unwrap().is_disabled());
            }
            // The zero-call assertions must not pass just because the hook is
            // disabled or globally paused: a clean substantial turn is admitted.
            session
                .agent
                .replace_messages(vec![Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new("x".repeat(500)))],
                    ..Default::default()
                }))]);
            let turn = AdvisorTurn::new(&session, Arc::new(|_| {}));
            session.maybe_advise_turn(None, &turn, false).await.unwrap();
            assert_eq!(advisor.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn advisor_absent_means_zero_overhead() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let agent = Agent::new(
                Arc::new(ScriptedDoerProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                }),
                tools,
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
            assert!(agent_session.advisor.is_none());
            let result = agent_session
                .run_text_with_abort("work".to_string(), None, |_| {})
                .await;
            assert!(result.is_ok(), "no advisor → turn unaffected");
        });
    }
}
