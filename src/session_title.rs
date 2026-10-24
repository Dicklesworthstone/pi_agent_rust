//! Best-effort session names from a bounded, private auxiliary completion.
//!
//! Preparation borrows the live agent and retains only screened text. The
//! interactive owner controls cancellation and publishes a name only after
//! its session metadata has been saved successfully.

use std::sync::Arc;
#[cfg(any(feature = "ftui", test))]
use std::sync::mpsc;
use std::time::Duration;

use asupersync::sync::OwnedMutexGuard;
#[cfg(any(feature = "ftui", test))]
use futures::future::Abortable;
use futures::future::{AbortHandle, AbortRegistration};

use crate::error::{Error, Result};
use crate::model::{ContentBlock, Message, StopReason, ThinkingLevel, UserContent, UserMessage};
use crate::provider::{Context, Provider, StreamOptions};
#[cfg(any(feature = "ftui", test))]
use crate::session::{Session, SessionEntry};
use crate::text_completion::{
    AuxiliaryPrivacy, MAX_INPUT_BYTES, collect_text, redact_inputs, with_timeout,
};

const SYSTEM_PROMPT: &str = "You name coding sessions. Reply with ONLY a short plain-text title: 3-7 words, no quotes, no markdown, no trailing punctuation.";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REPLY_BYTES: usize = 4096;
const MAX_INPUT_FIELDS: usize = 256;

/// An optional tiny/smol client bound to its launch-time destination and key.
pub struct TitleClient {
    provider: Arc<dyn Provider>,
    options: StreamOptions,
    privacy: Arc<AuxiliaryPrivacy>,
}

pub(crate) struct PreparedTitle {
    system_prompt: String,
    prompt: String,
    privacy: Arc<AuxiliaryPrivacy>,
}

impl TitleClient {
    /// The caller resolves only the tiny/smol role. Missing credentials or an
    /// unavailable provider silently disable this optional background request.
    pub fn for_model_entry(
        entry: &crate::models::ModelEntry,
        primary_entry: &crate::models::ModelEntry,
        cli_api_key: Option<&str>,
        auth: &crate::auth::AuthStorage,
        secrets: Option<&crate::secrets::SecretsSettings>,
    ) -> Option<Arc<Self>> {
        // An explicit foreground credential belongs to that configured
        // destination. A tiny/smol role on another provider or endpoint must
        // resolve its own credential rather than receiving that secret.
        let same_destination = crate::provider_metadata::provider_ids_match(
            &entry.model.provider,
            &primary_entry.model.provider,
        ) && entry.model.base_url == primary_entry.model.base_url;
        let api_key =
            crate::models::resolve_model_key(cli_api_key.filter(|_| same_destination), auth, entry);
        if crate::models::model_requires_configured_credential(entry) && api_key.is_none() {
            return None;
        }
        Some(Arc::new(Self {
            provider: crate::providers::create_provider_with_auth(entry, None, Some(auth)).ok()?,
            options: StreamOptions {
                api_key,
                headers: entry.headers.clone(),
                max_tokens: Some(entry.model.max_tokens.min(96)),
                thinking_level: Some(entry.clamp_thinking_level(ThinkingLevel::Minimal)),
                ..Default::default()
            },
            privacy: Arc::new(AuxiliaryPrivacy::from_settings(secrets)),
        }))
    }

    pub(crate) fn prepare_with_agent(
        &self,
        agent: &crate::agent::Agent,
    ) -> Result<Option<PreparedTitle>> {
        self.prepare_projected(agent.messages(), |parts| {
            agent.project_auxiliary_inputs(parts, &self.privacy)
        })
    }

    fn prepare_projected(
        &self,
        messages: &[Message],
        screen: impl FnOnce(&[&str]) -> Result<Vec<String>>,
    ) -> Result<Option<PreparedTitle>> {
        let Some((user, assistant)) = completed_exchange(messages) else {
            return Ok(None);
        };
        let mut fields = vec![SYSTEM_PROMPT];
        let mut bytes = SYSTEM_PROMPT.len();
        match user {
            UserContent::Text(text) => push_field(&mut fields, &mut bytes, text)?,
            UserContent::Blocks(blocks) => {
                for text in text_fields(blocks) {
                    push_field(&mut fields, &mut bytes, text)?;
                }
            }
        }
        let assistant_start = fields.len();
        for text in text_fields(&assistant.content) {
            push_field(&mut fields, &mut bytes, text)?;
        }
        if fields
            .iter()
            .take(assistant_start)
            .skip(1)
            .all(|text| text.trim().is_empty())
            || fields
                .iter()
                .skip(assistant_start)
                .all(|text| text.trim().is_empty())
        {
            return Ok(None);
        }
        // Preserve individual complete fields for anchored custom patterns.
        // Discover across ALL fields before joining or clipping, so a later
        // assignment also protects a bare echo before the excerpt cutoff.
        let protected = screen(&fields)?;
        if protected.len() != fields.len() {
            return Err(Error::validation(
                "session title privacy projection changed shape",
            ));
        }
        let changed_shape = || Error::validation("session title privacy projection changed shape");
        let (system_prompt, text) = protected.split_first().ok_or_else(changed_shape)?;
        let (user, assistant) = text
            .split_at_checked(assistant_start.saturating_sub(1))
            .ok_or_else(changed_shape)?;
        let user = user.join("\n");
        let assistant = assistant.join("\n");
        Ok(Some(PreparedTitle {
            system_prompt: system_prompt.clone(),
            prompt: format!(
                "Name this coding session.\n\nUser:\n{}\n\nAssistant (excerpt):\n{}",
                excerpt(&user, 2000),
                excerpt(&assistant, 800)
            ),
            privacy: Arc::clone(&self.privacy),
        }))
    }

    pub(crate) async fn generate(&self, request: PreparedTitle) -> Option<String> {
        self.generate_with_timeout(request, REQUEST_TIMEOUT).await
    }

    async fn generate_with_timeout(
        &self,
        request: PreparedTitle,
        timeout: Duration,
    ) -> Option<String> {
        if !Arc::ptr_eq(&request.privacy, &self.privacy) {
            return None;
        }
        let context = Context {
            system_prompt: Some(request.system_prompt.into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text(request.prompt),
                timestamp: chrono::Utc::now().timestamp_millis(),
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let raw = with_timeout(timeout, async {
            let stream = self.provider.stream(&context, &self.options).await?;
            collect_text(stream, MAX_REPLY_BYTES).await
        })
        .await
        .ok()?
        .ok()?;
        let protected = redact_inputs(&[&raw], &self.privacy).ok()?;
        sanitize(protected.first()?)
    }
}

fn completed_exchange(
    messages: &[Message],
) -> Option<(&UserContent, &crate::model::AssistantMessage)> {
    let mut users = messages.iter().filter_map(|message| match message {
        Message::User(user) => Some(&user.content),
        _ => None,
    });
    let user = users.next()?;
    if users.next().is_some() {
        return None;
    }
    let Message::Assistant(assistant) = messages.last()? else {
        return None;
    };
    // A safe run-boundary marker is not a completed provider response. In
    // particular, --max-time 0 must not start an auxiliary naming request.
    if assistant.api.is_empty()
        && assistant.provider.is_empty()
        && assistant.model.is_empty()
        && text_fields(&assistant.content).any(|text| text.starts_with("[time cap reached]"))
    {
        return None;
    }
    (assistant.stop_reason == StopReason::Stop
        && assistant.error_message.is_none()
        && !assistant
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_))))
    .then_some((user, assistant.as_ref()))
}

fn text_fields(blocks: &[ContentBlock]) -> impl Iterator<Item = &str> {
    blocks.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    })
}

fn push_field<'a>(fields: &mut Vec<&'a str>, bytes: &mut usize, text: &'a str) -> Result<()> {
    let Some(next_bytes) = bytes
        .checked_add(text.len())
        .filter(|total| *total <= MAX_INPUT_BYTES && fields.len() < MAX_INPUT_FIELDS)
    else {
        return Err(Error::validation(
            "PI_AUXILIARY_INPUT_LIMIT: session title input exceeds the privacy scan budget",
        ));
    };
    *bytes = next_bytes;
    fields.push(text);
    Ok(())
}

fn excerpt(text: &str, chars: usize) -> String {
    text.trim().chars().take(chars).collect()
}

fn sanitize(raw: &str) -> Option<String> {
    let first = raw.lines().next()?.trim();
    let clean: String = first
        .trim_matches(['"', '\'', '`', '*', '#'])
        .trim_end_matches(['.', '!', ':'])
        .chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    ch,
                    '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
        })
        .take(60)
        .collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_string())
}

/// Dropping the interactive owner drops its in-flight provider future too.
pub(crate) struct TitleCancellation(AbortHandle);

impl TitleCancellation {
    pub(crate) fn new() -> (Self, AbortRegistration) {
        let (handle, registration) = AbortHandle::new_pair();
        (Self(handle), registration)
    }

    pub(crate) fn handle(&self) -> AbortHandle {
        self.0.clone()
    }
}

impl Drop for TitleCancellation {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Save a private candidate before publishing metadata into the live session.
/// Shared admission and session locks make a manual name already present win.
pub(crate) async fn save_if_unnamed(
    agent_session: &crate::agent::AgentSession,
    owner_session_id: &str,
    title: &str,
    cancellation: Option<&AbortHandle>,
) -> Result<bool> {
    if !agent_session.save_enabled() || cancellation.is_some_and(AbortHandle::is_aborted) {
        return Ok(false);
    }
    let cx = crate::agent_cx::AgentCx::for_request();
    let gate = agent_session.provider_admission_gate();
    let _provider_permit = gate.acquire(cx.cx()).await?;
    gate.ensure_allowed()?;
    let actions = agent_session.session_action_admission_gate();
    let _action_permit = actions.acquire(cx.cx()).await?;
    let mut session = OwnedMutexGuard::lock(Arc::clone(&agent_session.session), cx.cx())
        .await
        .map_err(|error| Error::session(error.to_string()))?;
    if session.header.id != owner_session_id
        || session.get_name().is_some()
        || session.path.is_none()
        || session.autosave_metrics().pending_mutations != 0
        || cancellation.is_some_and(AbortHandle::is_aborted)
    {
        return Ok(false);
    }
    let mut candidate = session.clone();
    candidate.set_name(title);
    // Mark uncertainty immediately before persistence. If the owner is
    // dropped during the save, the shared gate remains closed. The identical
    // candidate reconciles an append that reached disk before a failed ack.
    gate.block("session title persistence was interrupted before live installation".to_string());
    if let Err(first_error) = candidate.save().await
        && let Err(retry_error) = candidate.save().await
    {
        let reason = format!(
            "session title persistence remained indeterminate after an idempotent retry: first failure: {first_error}; retry failure: {retry_error}"
        );
        gate.block(reason.clone());
        return Err(Error::session_persistence(reason));
    }
    *session = candidate;
    gate.clear();
    Ok(true)
}

#[cfg(any(feature = "ftui", test))]
struct PendingTitle {
    cancellation: TitleCancellation,
    result: mpsc::Receiver<Option<String>>,
}

/// FTUI's driver owns this controller. Binding a resumed nonempty conversation
/// prevents retroactive charges; binding a new empty session arms one attempt.
#[derive(Default)]
#[cfg(any(feature = "ftui", test))]
pub(crate) struct AutoTitleController {
    owner_session_id: Option<String>,
    attempted: bool,
    pending: Option<PendingTitle>,
}

#[cfg(any(feature = "ftui", test))]
impl AutoTitleController {
    pub(crate) fn for_session(session: &Session) -> Self {
        let mut controller = Self::default();
        controller.bind(session);
        controller
    }

    fn bind(&mut self, session: &Session) {
        if self.owner_session_id.as_deref() != Some(&session.header.id) {
            self.cancel_pending();
            self.owner_session_id = Some(session.header.id.clone());
            self.attempted = session
                .entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Message(_)));
        }
    }

    pub(crate) fn cancel_pending(&mut self) {
        self.pending = None;
    }

    pub(crate) async fn tick(
        &mut self,
        agent_session: &crate::agent::AgentSession,
        client: Option<&Arc<TitleClient>>,
        runtime: &asupersync::runtime::RuntimeHandle,
    ) -> Option<String> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let session = OwnedMutexGuard::lock(Arc::clone(&agent_session.session), cx.cx())
            .await
            .ok()?;
        self.bind(&session);
        if !agent_session.save_enabled()
            || session.get_name().is_some()
            || agent_session.ensure_provider_reentry_allowed().is_err()
        {
            self.cancel_pending();
            return None;
        }
        let ready = self
            .pending
            .as_ref()
            .and_then(|pending| match pending.result.try_recv() {
                Ok(title) => Some(title),
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(title) = ready {
            let pending = self.pending.take()?;
            let title = title?;
            let owner = self.owner_session_id.as_deref()?;
            let cancellation = pending.cancellation.handle();
            drop(session);
            return save_if_unnamed(agent_session, owner, &title, Some(&cancellation))
                .await
                .ok()?
                .then_some(title);
        }
        if self.attempted
            || session.path.is_none()
            || session.autosave_metrics().pending_mutations != 0
            || completed_exchange(agent_session.agent.messages()).is_none()
        {
            return None;
        }
        let client = Arc::clone(client?);
        self.attempted = true;
        let prepared = client.prepare_with_agent(&agent_session.agent).ok()??;
        drop(session);
        let (cancellation, registration) = TitleCancellation::new();
        let (sender, result) = mpsc::sync_channel(1);
        runtime.spawn(async move {
            if let Ok(title) = Abortable::new(client.generate(prepared), registration).await {
                let _ = sender.try_send(title);
            }
        });
        self.pending = Some(PendingTitle {
            cancellation,
            result,
        });
        None
    }
}

#[cfg(test)]
mod tests;
