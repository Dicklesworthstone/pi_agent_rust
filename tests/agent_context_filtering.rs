#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use futures::Stream;
    use pi::agent::{Agent, AgentConfig};
    use pi::model::{CustomMessage, Message, UserContent, UserMessage};
    use pi::provider::{Context, Provider, StreamOptions};
    use pi::tools::ToolRegistry;
    use std::pin::Pin;
    use std::sync::Arc;

    // Access private build_context via a test helper or just inspect the logic by simulating it?
    // Since build_context is private, we can't call it directly from integration tests without
    // exposing it or using a backdoor.
    // However, we can use the fact that run_loop calls build_context.
    // But we want to inspect the Context passed to the provider.

    // We can mock the provider and capture the context.

    use std::sync::Mutex;

    struct CapturingProvider {
        context: Arc<Mutex<Option<Vec<Message>>>>,
        name: String,
        api: String,
        model_id: String,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn api(&self) -> &str {
            &self.api
        }
        fn model_id(&self) -> &str {
            &self.model_id
        }
        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> pi::error::Result<
            Pin<Box<dyn Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>>,
        > {
            let mut guard = self.context.lock().unwrap();
            *guard = Some(context.messages.to_vec());
            drop(guard);
            // Return empty stream
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn test_hidden_custom_messages_filtered_from_context() {
        asupersync::test_utils::run_test(|| async {
            let captured_messages = Arc::new(Mutex::new(None));
            let provider = Arc::new(CapturingProvider {
                context: captured_messages.clone(),
                name: "capturing".to_string(),
                api: "capturing".to_string(),
                model_id: "capturing".to_string(),
            });

            let mut agent = Agent::new(
                provider,
                ToolRegistry::new(&[], std::path::Path::new("."), None),
                AgentConfig::default(),
            );

            // Add a visible user message
            agent.add_message(Message::User(UserMessage {
                content: UserContent::Text("visible".to_string()),
                timestamp: 0,
            }));

            // Add a hidden custom message
            agent.add_message(Message::Custom(CustomMessage {
                content: "hidden".to_string(),
                custom_type: "internal".to_string(),
                display: false,
                details: None,
                timestamp: 0,
            }));

            // Add a visible custom message
            agent.add_message(Message::Custom(CustomMessage {
                content: "visible_custom".to_string(),
                custom_type: "internal".to_string(),
                display: true, // Visible
                details: None,
                timestamp: 0,
            }));

            // Add pi's own hidden provenance record: its payload travels in
            // the system prompt, so it is the one hidden message the model
            // must not see twice.
            agent.add_message(Message::Custom(CustomMessage {
                content: "provenance".to_string(),
                custom_type: "semantic_context_bundle".to_string(),
                display: false,
                details: None,
                timestamp: 0,
            }));

            // Trigger a run to force context build
            // We use run_continue_with_abort just to trigger the loop without adding a new user prompt
            let _ = agent.run_continue_with_abort(None, |_| {}).await;

            let messages = captured_messages
                .lock()
                .unwrap()
                .clone()
                .expect("stream called");

            // `display: false` only hides a message from the TUI; extension
            // messages injected with it still reach the model. Only pi's
            // provenance record is excluded.
            assert_eq!(messages.len(), 3, "context: {messages:?}");

            // Check contents
            match &messages[0] {
                Message::User(u) => match &u.content {
                    UserContent::Text(text) => assert_eq!(text, "visible"),
                    UserContent::Blocks(_) => panic!("Expected text user content"),
                },
                _ => panic!("Expected user message"),
            }

            match &messages[1] {
                Message::Custom(c) => {
                    assert_eq!(c.content, "hidden");
                    assert!(!c.display);
                }
                _ => panic!("Expected the hidden custom message to reach the model"), // ubs:ignore test assertion
            }

            let visible_custom = &messages[2]; // ubs:ignore length asserted above
            match visible_custom {
                Message::Custom(c) => {
                    assert_eq!(c.content, "visible_custom");
                    assert!(c.display);
                }
                _ => panic!("Expected visible custom message"),
            }

            assert!(
                !messages.iter().any(|message| matches!(
                    message,
                    Message::Custom(c) if c.custom_type == "semantic_context_bundle"
                )),
                "provenance record must stay out of the provider context"
            );
        });
    }
}
