//! Native, caller-owned text sampling without transcript or tool inheritance.

use super::AgentToolInvocationHost;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ThinkingLevel,
    UserContent, UserMessage,
};
use crate::provider::Context;
use crate::tools::{ToolSamplingModel, ToolSamplingRequest, ToolSamplingResult, ToolSamplingRole};
use futures::{Stream, StreamExt};
use std::sync::atomic::Ordering;

const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_TOKENS: u32 = 65_536;
const MAX_MESSAGES: usize = 64;
const MAX_INPUT_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_STREAM_EVENTS: usize = 65_536;
const READY_EVENT_BATCH: usize = 64;

fn refused(message: &'static str) -> Error {
    Error::tool("mcp", message)
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(refused("sampling requires the caller's I/O and timer capabilities"));
    }
    owner.checkpoint().map_err(|_| refused("sampling was cancelled"))
}

pub(super) fn model(host: &AgentToolInvocationHost<'_>) -> Result<ToolSamplingModel> {
    if host.revoked.load(Ordering::Acquire) {
        return Err(refused("sampling invocation was revoked"));
    }
    let options = &host.agent.config.stream_options;
    let session_id = options.session_id.as_ref()
        .filter(|session| !session.trim().is_empty())
        .ok_or_else(|| refused("sampling requires an active session"))?;
    let provider = &host.agent.provider;
    if provider.name().is_empty() || provider.model_id().is_empty() {
        return Err(refused("sampling requires an active model"));
    }
    Ok(ToolSamplingModel {
        provider: provider.name().to_string(),
        model: provider.model_id().to_string(),
        session_id: session_id.clone(),
        max_tokens: Some(options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).min(MAX_TOKENS)),
        temperature: options.temperature,
    })
}

fn validate(request: &ToolSamplingRequest, model: &ToolSamplingModel) -> Result<()> {
    if request.max_tokens == 0
        || request.max_tokens > model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
        || request.temperature.is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
        || request.messages.is_empty()
        || request.messages.len() > MAX_MESSAGES
    {
        return Err(refused("sampling request exceeds the approved model limits"));
    }
    request.messages.iter().map(|message| message.text.len())
        .chain(request.system_prompt.iter().map(String::len))
        .try_fold(0usize, |total, bytes| total.checked_add(bytes).filter(|sum| *sum <= MAX_INPUT_BYTES))
        .ok_or_else(|| refused("sampling prompt exceeds the input byte limit"))?;
    Ok(())
}

async fn cancellation(owner: AgentCx) {
    let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
    let _ = receiver.recv(owner.cx()).await;
    drop(sender);
}

pub(super) async fn check_session(
    host: &AgentToolInvocationHost<'_>,
    expected_session: &str,
) -> Result<()> {
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    if model(host)?.session_id != expected_session {
        return Err(refused("sampling session changed"));
    }
    owner.with_current(host.check_session(expected_session)).await?;
    check_owner(&owner)
}

pub(super) async fn sample(
    host: &AgentToolInvocationHost<'_>,
    request: ToolSamplingRequest,
    expected_session: &str,
) -> Result<ToolSamplingResult> {
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    let selected = model(host)?;
    if selected.session_id != expected_session {
        return Err(refused("sampling session changed before generation"));
    }
    validate(&request, &selected)?;
    // Keep the original owner installed for provider polling and stream drop.
    // No new retry or timeout budget is created inside the MCP call deadline.
    let operation = owner.with_current(async {
        host.check_session(expected_session).await?;
        check_owner(&owner)?;
        let timestamp = chrono::Utc::now().timestamp_millis();
        let messages = request.messages.into_iter().map(|message| match message.role {
            ToolSamplingRole::User => Message::User(UserMessage {
                content: UserContent::Text(message.text),
                timestamp,
            }),
            ToolSamplingRole::Assistant => Message::assistant(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(message.text))],
                provider: selected.provider.clone(),
                model: selected.model.clone(),
                api: host.agent.provider.api().to_string(),
                timestamp,
                ..Default::default()
            }),
        }).collect();
        let context = Context::owned(request.system_prompt, messages, Vec::new());
        let mut options = host.agent.config.stream_options.clone();
        options.max_tokens = Some(request.max_tokens);
        options.temperature = request.temperature;
        options.thinking_level = Some(ThinkingLevel::Off);
        options.thinking_budgets = None;
        options.before_provider_request = None;
        // Provider continuation/cache affinity must not merge this server's
        // isolated prompt into the Agent's conversation or a previous sample.
        let sampling_id = format!("mcp-sampling-{}", uuid::Uuid::new_v4().simple());
        options.session_id = Some(sampling_id.clone());
        // Per-request headers win over provider compatibility headers. Remove
        // alternate casing so a configured conversation ID cannot override
        // the isolated affinity on the Responses/Codex transport.
        options.headers.retain(|key, _| !key.eq_ignore_ascii_case("session_id"));
        options.headers.insert("session_id".to_string(), sampling_id.clone());
        options.prompt_cache_key = options.prompt_cache_key.map(|_| sampling_id);
        // Use the installed wrapper: it owns the original session's pricing,
        // cap and partial-usage settlement even if this future is dropped.
        let stream = host.agent.provider.stream(&context, &options).await?;
        let reply = collect(stream, &owner).await?;
        check_owner(&owner)?;
        host.check_session(expected_session).await?;
        check_owner(&owner)?;
        Ok(reply)
    });
    let result = match futures::future::select(
        Box::pin(operation), Box::pin(cancellation(owner.clone())),
    ).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), pending)) => {
            drop(pending);
            Err(refused("sampling was cancelled"))
        }
    };
    host.agent.emit_cost_warning(&host.on_event);
    result
}

async fn collect<S>(mut stream: S, owner: &AgentCx) -> Result<ToolSamplingResult>
where
    S: Stream<Item = Result<StreamEvent>> + Unpin,
{
    let mut events = 0usize;
    let mut delta_bytes = 0usize;
    loop {
        check_owner(owner)?;
        let Some(event) = stream.next().await else {
            return Err(refused("sampling stream ended without a terminal response"));
        };
        events += 1;
        if events > MAX_STREAM_EVENTS {
            return Err(refused("sampling exceeded its stream event limit"));
        }
        match event? {
            StreamEvent::Start { partial } => bounded_message(&partial)?,
            StreamEvent::TextDelta { delta, .. } | StreamEvent::ThinkingDelta { delta, .. } => {
                delta_bytes = delta_bytes.checked_add(delta.len())
                    .filter(|bytes| *bytes <= MAX_OUTPUT_BYTES)
                    .ok_or_else(|| refused("sampling exceeded its output byte limit"))?;
            }
            StreamEvent::TextEnd { content, .. } | StreamEvent::ThinkingEnd { content, .. }
                if content.len() > MAX_OUTPUT_BYTES => {
                return Err(refused("sampling exceeded its output byte limit"));
            }
            StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallDelta { .. }
            | StreamEvent::ToolCallEnd { .. } => {
                return Err(refused("sampling returned an unrequested tool call"));
            }
            StreamEvent::Error { .. } => {
                return Err(refused("sampling provider reported a failed response"));
            }
            StreamEvent::Done { reason, message } => {
                if !matches!(reason, StopReason::Stop | StopReason::Length)
                    || reason != message.stop_reason || message.error_message.is_some()
                {
                    return Err(refused("sampling returned an unsupported terminal response"));
                }
                bounded_message(&message)?;
                let mut text = String::new();
                for block in message.content {
                    match block {
                        ContentBlock::Text(part) => {
                            if part.text.len() > MAX_OUTPUT_BYTES.saturating_sub(text.len()) {
                                return Err(refused("sampling exceeded its output byte limit"));
                            }
                            text.push_str(&part.text);
                        }
                        ContentBlock::Thinking(_) => {}
                        _ => return Err(refused("sampling returned unsupported response content")),
                    }
                }
                if text.trim().is_empty() {
                    return Err(refused("sampling returned an empty response"));
                }
                check_owner(owner)?;
                return Ok(ToolSamplingResult { text, stop_reason: reason });
            }
            _ => {}
        }
        if events.is_multiple_of(READY_EVENT_BATCH) {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            }).await;
        }
    }
}

fn bounded_message(message: &AssistantMessage) -> Result<()> {
    if message.content.len() > MAX_MESSAGES {
        return Err(refused("sampling exceeded its content block limit"));
    }
    let mut bytes = 0usize;
    for block in &message.content {
        let length = match block {
            ContentBlock::Text(text) => text.text.len(),
            ContentBlock::Thinking(thinking) => thinking.thinking.len(),
            _ => return Err(refused("sampling returned unsupported response content")),
        };
        bytes = bytes.checked_add(length)
            .filter(|total| *total <= MAX_OUTPUT_BYTES)
            .ok_or_else(|| refused("sampling exceeded its output byte limit"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentEvent};
    use crate::cost::CostBudget;
    use crate::provider::{BeforeProviderRequestHook, Provider, StreamOptions, ThinkingBudgets};
    use crate::tools::{ToolInvocationHost, ToolRegistry, ToolSamplingMessage};
    use async_trait::async_trait;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll, Waker};

    struct SamplingProbe {
        calls: Mutex<Vec<(Context<'static>, StreamOptions)>>,
        events: Vec<StreamEvent>,
        pending: bool,
    }

    #[async_trait]
    impl Provider for SamplingProbe {
        fn name(&self) -> &'static str { "sampling-probe" }
        fn api(&self) -> &'static str { "sampling-test" }
        fn model_id(&self) -> &'static str { "current-model" }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            self.calls.lock().unwrap().push((Context::owned(
                context.system_prompt.as_deref().map(str::to_string),
                context.messages.to_vec(),
                context.tools.to_vec(),
            ), options.clone()));
            let events = futures::stream::iter(self.events.clone().into_iter().map(Ok));
            if self.pending {
                Ok(Box::pin(events.chain(futures::stream::pending())))
            } else {
                Ok(Box::pin(events))
            }
        }
    }

    fn terminal(reason: StopReason, text: &str, cost: f64) -> StreamEvent {
        let mut message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            stop_reason: reason,
            ..Default::default()
        };
        message.usage.cost.total = cost;
        StreamEvent::Done { reason, message }
    }

    fn request() -> ToolSamplingRequest {
        ToolSamplingRequest {
            system_prompt: Some("Reviewed server instructions".to_string()),
            messages: vec![
                ToolSamplingMessage { role: ToolSamplingRole::User, text: "Reviewed question".into() },
                ToolSamplingMessage { role: ToolSamplingRole::Assistant, text: "Reviewed prior answer".into() },
            ],
            max_tokens: 128,
            temperature: Some(0.25),
        }
    }

    fn agent(events: Vec<StreamEvent>, pending: bool, budget: Option<CostBudget>) -> (Agent, Arc<SamplingProbe>) {
        let probe = Arc::new(SamplingProbe { calls: Mutex::new(Vec::new()), events, pending });
        let mut agent = Agent::new(probe.clone(), ToolRegistry::new(&[], Path::new("."), None), AgentConfig {
            system_prompt: Some("PRIVATE HOST SYSTEM".into()),
            stream_options: StreamOptions {
                session_id: Some("sampling-owner".into()),
                prompt_cache_key: Some("private-host-affinity".into()),
                max_tokens: Some(256),
                api_key: Some("private-provider-credential".into()),
                headers: std::collections::HashMap::from([
                    ("SESSION_ID".into(), "private-host-header".into()),
                    ("Authorization".into(), "Bearer private-header-credential".into()),
                ]),
                thinking_level: Some(ThinkingLevel::High),
                thinking_budgets: Some(ThinkingBudgets::default()),
                before_provider_request: Some(BeforeProviderRequestHook::new(|_| {
                    panic!("sampling must not invoke the main conversation rewrite hook")
                })),
                ..Default::default()
            },
            cost_budget: budget,
            ..Default::default()
        });
        agent.job_session_scope = crate::jobs::JobSessionScope::fixed("sampling-owner");
        agent.messages.push(Message::User(UserMessage {
            content: UserContent::Text("PRIVATE HOST TRANSCRIPT".into()), timestamp: 0,
        }));
        (agent, probe)
    }

    #[test]
    fn native_sampling_isolates_prompt_and_affinity_but_shares_the_cost_guard() {
        futures::executor::block_on(async {
            let budget = CostBudget::new(Some(1.0), Some(2.0), 0.0).unwrap();
            let (agent, probe) = agent(vec![terminal(StopReason::Length, "Limited answer", 2.0)], false, Some(budget.clone()));
            let warnings = Arc::new(Mutex::new(Vec::new()));
            let captured = warnings.clone();
            let host = AgentToolInvocationHost::new(&agent, Arc::new(move |event| {
                if let AgentEvent::CostWarning { spent, .. } = event { captured.lock().unwrap().push(spent); }
            }));
            let owner = AgentCx::for_request();
            let result = owner.with_current(host.sample(request(), "sampling-owner")).await.unwrap();
            assert_eq!(result.text, "Limited answer");
            assert_eq!(result.stop_reason, StopReason::Length);
            assert!((budget.spent() - 2.0).abs() < f64::EPSILON);
            assert_eq!(*warnings.lock().unwrap(), vec![2.0]);
            assert_eq!(agent.messages.len(), 1, "sampling cannot mutate the Agent transcript");
            {
                let calls = probe.calls.lock().unwrap();
                assert_eq!(calls.len(), 1);
                let (context, options) = &calls[0];
                assert_eq!(context.system_prompt.as_deref(), Some("Reviewed server instructions"));
                assert_eq!(context.messages.len(), 2);
                assert!(context.tools.is_empty());
                let Message::User(user) = &context.messages[0] else { panic!("requested user role"); };
                assert!(matches!(&user.content, UserContent::Text(text) if text == "Reviewed question"));
                let Message::Assistant(assistant) = &context.messages[1] else { panic!("requested assistant role"); };
                assert_eq!(assistant.model, "current-model");
                assert_eq!(options.max_tokens, Some(128));
                assert_eq!(options.temperature, Some(0.25));
                assert_eq!(options.api_key.as_deref(), Some("private-provider-credential"));
                assert_eq!(options.thinking_level, Some(ThinkingLevel::Off));
                assert!(options.thinking_budgets.is_none() && options.before_provider_request.is_none());
                assert!(options.session_id.as_deref().unwrap().starts_with("mcp-sampling-"));
                assert_eq!(options.prompt_cache_key, options.session_id);
                assert_eq!(options.headers.get("session_id"), options.session_id.as_ref());
                assert!(!options.headers.contains_key("SESSION_ID"));
                assert_eq!(options.headers.get("Authorization").map(String::as_str), Some("Bearer private-header-credential"));
            }
            assert!(owner.with_current(host.sample(request(), "sampling-owner")).await.is_err());
            assert_eq!(probe.calls.lock().unwrap().len(), 1, "cost refusal precedes provider I/O");
        });
    }

    #[test]
    fn ownership_fences_follow_the_live_session_even_when_model_options_are_unchanged() {
        futures::executor::block_on(async {
            let (agent, probe) = agent(vec![terminal(StopReason::Stop, "Reply", 0.0)], false, None);
            let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {}));
            let owner = AgentCx::for_request();
            owner.with_current(host.check_sampling_session("sampling-owner")).await.unwrap();
            agent.job_session_scope.bind(Arc::new(|| Box::pin(async { Some("different-session".into()) })));
            assert_eq!(host.sampling_model().unwrap().session_id, "sampling-owner");
            assert!(owner.with_current(host.check_sampling_session("sampling-owner")).await.is_err());
            assert!(host.sampling_model().is_err(), "a changed owner permanently revokes this host");
            assert!(owner.with_current(host.sample(request(), "sampling-owner")).await.is_err());
            assert!(probe.calls.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn cancelled_and_dropped_samples_retain_acknowledged_partial_cost_once() {
        for cancel in [false, true] {
            let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
            let mut partial = AssistantMessage::default();
            partial.usage.cost.total = 1.25;
            let (agent, probe) = agent(vec![StreamEvent::Start { partial }], true, Some(budget.clone()));
            let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {}));
            let owner = AgentCx::for_request();
            let mut pending = Box::pin(owner.with_current(host.sample(request(), "sampling-owner")));
            let mut task = TaskContext::from_waker(Waker::noop());
            assert!(pending.as_mut().poll(&mut task).is_pending());
            assert_eq!(probe.calls.lock().unwrap().len(), 1);
            assert!((budget.snapshot().pending - 1.25).abs() < f64::EPSILON);
            if cancel {
                owner.cancel_with(asupersync::types::CancelKind::User, Some("sampling test"));
                assert!(matches!(pending.as_mut().poll(&mut task), Poll::Ready(Err(_))));
            }
            drop(pending);
            assert!((budget.spent() - 1.25).abs() < f64::EPSILON);
            assert_eq!(budget.snapshot().pending, 0.0);
        }
    }

    #[test]
    fn unbound_sessions_and_invalid_requests_never_reach_the_provider() {
        futures::executor::block_on(async {
            let (mut agent, probe) = agent(Vec::new(), false, None);
            agent.config.stream_options.session_id = None;
            let owner = AgentCx::for_request();
            {
                let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {}));
                assert!(host.sampling_model().is_err());
                assert!(owner.with_current(host.sample(request(), "sampling-owner")).await.is_err());
            }
            agent.config.stream_options.session_id = Some("sampling-owner".into());
            let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {}));
            for invalid in [
                ToolSamplingRequest { max_tokens: 257, ..request() },
                ToolSamplingRequest { temperature: Some(f32::NAN), ..request() },
                ToolSamplingRequest { messages: Vec::new(), ..request() },
                ToolSamplingRequest { system_prompt: Some("x".repeat(MAX_INPUT_BYTES + 1)), ..request() },
            ] {
                assert!(owner.with_current(host.sample(invalid, "sampling-owner")).await.is_err());
            }
            assert!(probe.calls.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn cancellation_during_the_final_session_lookup_withholds_the_charged_reply() {
        futures::executor::block_on(async {
            let budget = CostBudget::new(None, None, 0.0).unwrap();
            let (agent, probe) = agent(
                vec![terminal(StopReason::Stop, "must be withheld", 0.5)],
                false,
                Some(budget.clone()),
            );
            let owner = AgentCx::for_request();
            let lookup_owner = owner.clone();
            let lookups = std::sync::atomic::AtomicUsize::new(0);
            agent.job_session_scope.bind(Arc::new(move || {
                if lookups.fetch_add(1, Ordering::SeqCst) == 1 {
                    lookup_owner.cancel_with(
                        asupersync::types::CancelKind::User,
                        Some("cancel during final session lookup"),
                    );
                }
                Box::pin(async { Some("sampling-owner".to_string()) })
            }));
            let host = AgentToolInvocationHost::new(&agent, Arc::new(|_| {}));
            assert!(owner.with_current(host.sample(request(), "sampling-owner")).await.is_err());
            assert_eq!(probe.calls.lock().unwrap().len(), 1);
            assert!((budget.spent() - 0.5).abs() < f64::EPSILON);
        });
    }

    #[test]
    fn incomplete_mismatched_tool_and_oversized_responses_are_not_published() {
        futures::executor::block_on(async {
            let owner = AgentCx::for_request();
            let mut mismatched = terminal(StopReason::Length, "Partial", 0.0);
            if let StreamEvent::Done { message, .. } = &mut mismatched { message.stop_reason = StopReason::Stop; }
            for events in [
                vec![StreamEvent::TextDelta { content_index: 0, delta: "partial without terminal".into() }],
                vec![mismatched],
                vec![terminal(StopReason::ToolUse, "unrequested tool", 0.0)],
                vec![terminal(StopReason::Stop, &"x".repeat(MAX_OUTPUT_BYTES + 1), 0.0)],
                vec![StreamEvent::ThinkingDelta { content_index: 0, delta: "x".repeat(MAX_OUTPUT_BYTES + 1) }],
                vec![StreamEvent::ToolCallStart { content_index: 0, id: "call".into(), name: "bash".into() }],
            ] {
                assert!(collect(futures::stream::iter(events.into_iter().map(Ok)), &owner).await.is_err());
            }
        });
    }

    #[test]
    fn a_ready_stream_yields_and_observes_owner_cancellation() {
        let owner = AgentCx::for_request();
        let events = (0..READY_EVENT_BATCH * 2).map(|_| Ok(StreamEvent::TextStart { content_index: 0 }));
        let mut pending = Box::pin(collect(futures::stream::iter(events), &owner));
        let mut task = TaskContext::from_waker(Waker::noop());
        assert!(pending.as_mut().poll(&mut task).is_pending());
        owner.cancel_with(asupersync::types::CancelKind::User, Some("ready stream"));
        assert!(matches!(pending.as_mut().poll(&mut task), Poll::Ready(Err(_))));
    }
}
