#![recursion_limit = "256"]

//! Imported Pi-AI completions must carry structured history and declared tools
//! to the requested host-registered model, using that model's credentials.

mod common;

use asupersync::sync::Mutex;
use async_trait::async_trait;
use common::harness::MockHttpResponse;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::{AuthCredential, AuthStorage};
use pi::compaction::ResolvedCompactionSettings;
use pi::model::StreamEvent;
use pi::models::{ModelEntry, ModelRegistry};
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct CurrentProvider(Arc<AtomicUsize>);

#[async_trait]
impl Provider for CurrentProvider {
    fn name(&self) -> &'static str {
        "completion-source"
    }

    fn api(&self) -> &'static str {
        "test-api"
    }

    fn model_id(&self) -> &'static str {
        "source-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(pi::error::Error::provider(
            "completion-source",
            "explicit target must not silently dispatch to the active provider",
        ))
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn imported_pi_ai_completion_uses_registered_target_and_real_structured_request() {
    let harness = common::TestHarness::new("pi_ai_native_completion");
    let server = harness.start_mock_http_server();
    let response = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"routed\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":14,\"completion_tokens\":5,\"total_tokens\":19}}\n\n",
        "data: [DONE]\n\n",
    );
    server.add_route(
        "POST",
        "/v1/chat/completions",
        MockHttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: response.as_bytes().to_vec(),
        },
    );
    let extension = harness.create_file(
        "native-completion.mjs",
        br"
        import { completeSimple, getModels } from '@mariozechner/pi-ai';
        export default function(pi) {
          pi.registerCommand('native-completion', { handler: async () => {
            const models = await getModels();
            const model = models.find(model => model.provider === 'completion-target' && model.id === 'target-model');
            if (!model) throw new Error('registered host model missing from Pi-AI catalog');
            const result = await completeSimple({
              ...model,
              api: 'anthropic-messages', baseUrl: 'https://caller-route.invalid',
              apiKey: 'caller-model-key', headers: {'X-Caller-Secret': 'caller-model-header'}
            }, {
              systemPrompt: 'Inspect the tool output and image',
              messages: [
                {role:'user', content:[
                  {type:'text', text:'Caption this'},
                  {type:'image', data:'aW1hZ2U=', mimeType:'image/png'}
                ], timestamp:1},
                {role:'assistant', provider:'completion-target', model:'target-model',
                 api:'openai-completions', stopReason:'toolUse', timestamp:2, content:[
                  {type:'text', text:'Looking it up', textSignature:'history-signature'},
                  {type:'toolCall', id:'lookup-1', name:'lookup', arguments:{key:'caption'}}
                ]},
                {role:'toolResult', toolCallId:'lookup-1', toolName:'lookup',
                 isError:false, timestamp:3, content:[{type:'text', text:'An image caption'}]}
              ],
              tools:[{name:'lookup', description:'Find a caption', parameters:{
                type:'object', properties:{key:{type:'string'}}, required:['key']
              }}]
            }, {
              maxTokens:41, temperature:0.25,
              apiKey:'caller-option-key', headers:{'X-Caller-Secret':'caller-option-header'}
            });
            return {provider:result.provider, model:result.model, api:result.api,
              content:result.content, usage:result.usage};
          }});
        }
        ",
    );
    let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
    registry.merge_entries(vec![ModelEntry {
        model: Model {
            id: "target-model".to_string(),
            name: "Registered target".to_string(),
            provider: "completion-target".to_string(),
            api: "openai-completions".to_string(),
            base_url: format!("{}/v1", server.base_url()),
            reasoning: false,
            input: vec![InputType::Text, InputType::Image],
            cost: ModelCost::default(),
            context_window: 32_768,
            max_tokens: 2048,
            headers: HashMap::new(),
        },
        api_key: Some("stale-catalog-key".to_string()),
        headers: HashMap::from([("X-Target-Route".to_string(), "registered".to_string())]),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }]);
    let mut auth = AuthStorage::empty_at(harness.temp_dir().join("auth.json"));
    auth.set(
        "completion-target",
        AuthCredential::ApiKey {
            key: "fresh-target-key".to_string(),
        },
    );
    let source_calls = Arc::new(AtomicUsize::new(0));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let agent = Agent::new(
            Arc::new(CurrentProvider(Arc::clone(&source_calls))),
            ToolRegistry::new(&[], harness.temp_dir(), None),
            AgentConfig {
                stream_options: StreamOptions {
                    api_key: Some("source-private-key".to_string()),
                    headers: HashMap::from([(
                        "X-Source-Secret".to_string(),
                        "source-only".to_string(),
                    )]),
                    ..StreamOptions::default()
                },
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(Session::in_memory())),
            false,
            ResolvedCompactionSettings::default(),
        )
        .with_model_registry(registry)
        .with_auth_storage(auth);
        session
            .enable_extensions(&[], harness.temp_dir(), None, &[extension])
            .await
            .expect("load imported Pi-AI completion command");
        let result = session
            .execute_extension_command("native-completion", "", 10_000, |_| {})
            .await
            .expect("native completion through registered target");
        assert_eq!(session.agent.provider().name(), "completion-source");
        assert!(
            session
                .extensions
                .as_ref()
                .expect("extension region")
                .shutdown()
                .await
        );
        result
    });
    assert_eq!(source_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result["provider"], "completion-target");
    assert_eq!(result["model"], "target-model");
    assert_eq!(result["api"], "openai-completions");
    assert_eq!(result["content"][0]["text"], "routed");
    assert_eq!(result["usage"]["totalTokens"], 19);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer fresh-target-key"));
    assert_eq!(header("x-target-route"), Some("registered"));
    assert_eq!(header("x-source-secret"), None);
    assert_eq!(header("x-caller-secret"), None);
    let body: Value = serde_json::from_slice(&request.body).expect("OpenAI request JSON");
    assert_eq!(body["model"], "target-model");
    assert_eq!(body["max_tokens"], 41);
    assert_eq!(body["temperature"], 0.25);
    assert_eq!(
        body["messages"][0]["content"],
        "Inspect the tool output and image"
    );
    assert_eq!(
        body["messages"][1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["tool_call_id"], "lookup-1");
    assert_eq!(body["messages"][3]["content"], "An image caption");
    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["required"],
        json!(["key"])
    );
}
