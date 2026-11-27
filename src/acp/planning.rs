//! Editor plan selection and exact-proposal review through ACP v1.
//!
//! Mode selection is a session operation: reserve before cancelling a running
//! prompt, wait for its durable owner cleanup, then change the native SDK state.
//! Reviewing a proposal never grants permission for its implementation tools.

use super::*;
use crate::model::{AssistantMessage, StopReason};
use crate::plan::{PlanChange, PlanMode, PlanPersistence, PlanState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Code,
    Plan,
}

impl Mode {
    fn parse(value: Option<&str>) -> std::result::Result<Self, &'static str> {
        match value {
            Some("code") => Ok(Self::Code),
            Some("plan") => Ok(Self::Plan),
            _ => Err("Mode must be 'code' or 'plan'"),
        }
    }

    const fn from_native(mode: PlanMode) -> Self {
        match mode {
            PlanMode::Planning | PlanMode::PendingApproval => Self::Plan,
            PlanMode::Off | PlanMode::Approved => Self::Code,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Plan => "plan",
        }
    }
}

pub(super) fn enabled(handle: &AgentSessionHandle) -> bool {
    !handle.session().agent.shared_tools().snapshot().builtins_disabled()
}

fn choices() -> Value {
    json!([
        {
            "id": "code",
            "name": "Code",
            "description": "Implement changes with permission for each tool."
        },
        {
            "id": "plan",
            "name": "Plan",
            "description": "Explore without mutations and submit a plan for review."
        }
    ])
}

pub(super) fn modes(handle: &AgentSessionHandle) -> Option<Value> {
    enabled(handle).then(|| json!({
        "currentModeId": Mode::from_native(handle.session().agent.plan_state().mode()).id(),
        "availableModes": choices(),
    }))
}

pub(super) fn append_option(options: &mut Value, state: &PlanState, available: bool) {
    if !available {
        return;
    }
    let entries = options.as_array_mut().expect("ACP config options are an array");
    entries.push(json!({
        "id": "mode",
        "name": "Session Mode",
        "category": "mode",
        "type": "select",
        "currentValue": Mode::from_native(state.mode()).id(),
        "options": [
            {
                "value": "code", "name": "Code",
                "description": "Implement changes with permission for each tool."
            },
            {
                "value": "plan", "name": "Plan",
                "description": "Explore without mutations and submit a plan for review."
            }
        ],
    }));
}

pub(super) fn current_mode(state: &PlanState) -> &'static str {
    Mode::from_native(state.mode()).id()
}

pub(super) fn mode_update(session_id: &str, mode: PlanMode) -> String {
    json_rpc_notification("session/update", json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "current_mode_update",
            "currentModeId": Mode::from_native(mode).id(),
        }
    }))
}

impl AcpOutput {
    pub(super) fn session_is_busy(&self, session_id: &str) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.prompts.contains_key(session_id) || state.mode_changes.contains(session_id)
    }

    fn reserve_mode_change(&self, session_id: &str) -> Result<ModeChangeGuard> {
        let mut state = self.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping || state.failure.is_some() {
            return Err(Error::session("ACP session is shutting down"));
        }
        if !state.mode_changes.insert(session_id.to_string()) {
            return Err(Error::session("A session mode change is already in progress"));
        }
        Ok(ModeChangeGuard {
            out: self.clone(),
            session_id: session_id.to_string(),
            started: false,
            settled: false,
        })
    }

    fn mode_change_admitted(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.stopping && state.failure.is_none()
    }
}

struct ModeChangeGuard {
    out: AcpOutput,
    session_id: String,
    started: bool,
    settled: bool,
}

impl Drop for ModeChangeGuard {
    fn drop(&mut self) {
        let mut state = self.out.shared.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.mode_changes.remove(&self.session_id);
        if self.started && !self.settled {
            self.out.shared.record_failure(
                &mut state,
                "ACP mode change ended before session cleanup completed",
            );
        }
    }
}

/// Keep the dispatcher available for cancelled permission, file and terminal
/// replies while the previous turn returns its session owner.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch(
    params: &Value,
    id: Value,
    config_surface: bool,
    initialized: bool,
    sessions: &AcpSessionsMap,
    cx: &AgentCx,
    out: &AcpOutput,
    runtime: &RuntimeHandle,
) {
    if !initialized {
        let _ = out.send(json_rpc_error(id, INVALID_REQUEST, "Server not initialized"));
        return;
    }
    let Some(session_id) = params.get("sessionId").and_then(Value::as_str).filter(|id| !id.trim().is_empty()) else {
        let _ = out.send(json_rpc_error(id, INVALID_PARAMS, "Missing required parameter: sessionId"));
        return;
    };
    let value = params.get(if config_surface { "value" } else { "modeId" }).and_then(Value::as_str);
    let mode = match Mode::parse(value) {
        Ok(mode) => mode,
        Err(message) => {
            let _ = out.send(json_rpc_error(id, INVALID_PARAMS, message));
            return;
        }
    };
    let state = match sessions.lock(cx).await {
        Ok(sessions) => sessions.get(session_id).cloned(),
        Err(_) => {
            let _ = out.send(json_rpc_error(id, INTERNAL_ERROR, "Session registry is unavailable"));
            return;
        }
    };
    let Some(state) = state else {
        let _ = out.send(json_rpc_error(id, SESSION_NOT_FOUND, "Session not found"));
        return;
    };
    let mut reservation = match out.reserve_mode_change(session_id) {
        Ok(reservation) => reservation,
        Err(error) => {
            let _ = out.send(json_rpc_error(id, PROMPT_IN_PROGRESS, error.to_string()));
            return;
        }
    };
    // Reservation precedes cancellation: another prompt cannot acquire the
    // session in the interval between the old turn and the new mode.
    out.abort_prompt(session_id);
    let session_id = session_id.to_string();
    let output = out.clone();
    let owner = cx.clone();
    let response_id = id.clone();
    let spawned = runtime.try_spawn(async move {
        reservation.started = true;
        let wait = async {
            while output.prompt_is_active(&session_id) {
                sleep(wall_now(), Duration::from_millis(5)).await;
            }
        };
        let result = match timeout(wall_now(), Duration::from_secs(30), Box::pin(wait)).await {
            Ok(()) => apply_selection(state, mode, &session_id, &owner, &output).await,
            Err(_) => Err(JsonRpcError {
                code: PROMPT_IN_PROGRESS,
                message: "The previous prompt has not completed its cleanup; mode was not changed".into(),
                data: None,
            }),
        };
        let response = match result {
            Ok(configuration) => json_rpc_ok(
                response_id,
                if config_surface { json!({"configOptions": configuration}) } else { json!({}) },
            ),
            Err(error) => serde_json::to_string(&JsonRpcResponse {
                jsonrpc: "2.0".into(), id: response_id, result: None, error: Some(error),
            }).expect("serialize mode response"),
        };
        reservation.settled = true;
        drop(reservation);
        let _ = output.send(response);
    });
    if spawned.is_err() {
        let _ = out.send(json_rpc_error(id, INTERNAL_ERROR, "Cannot start ACP mode change"));
    }
}

async fn apply_selection(
    state: Arc<Mutex<AcpSessionState>>,
    mode: Mode,
    session_id: &str,
    cx: &AgentCx,
    out: &AcpOutput,
) -> std::result::Result<Value, JsonRpcError> {
    let failure = |code, message: &str| JsonRpcError { code, message: message.into(), data: None };
    if !out.mode_change_admitted() || cx.is_cancel_requested() {
        return Err(failure(INVALID_REQUEST, "Session mode change was cancelled"));
    }
    if out.shared.files.has_write(session_id) || out.shared.terminals.has_unsettled(session_id) {
        return Err(failure(PROMPT_IN_PROGRESS, "Waiting for editor file writes or terminal cleanup before changing mode"));
    }
    let mut state = OwnedMutexGuard::lock(state, cx).await
        .map_err(|_| failure(INTERNAL_ERROR, "Session state is unavailable"))?;
    let available_models = state.available_models.clone();
    let config = Arc::clone(&state.config);
    let cwd = state.cwd.clone();
    let handle = state.agent_session.as_mut()
        .ok_or_else(|| failure(PROMPT_IN_PROGRESS, "Session owner has not finished its prompt"))?;
    if !enabled(handle) {
        return Err(failure(INVALID_PARAMS, "Session modes are unavailable while built-in tools are disabled"));
    }
    handle.session().ensure_provider_reentry_allowed()
        .map_err(|_| failure(INTERNAL_ERROR, "Session persistence is unconfirmed; resume the saved session before changing mode"))?;
    let current = handle.session().agent.plan_state().mode();
    let change = if Mode::from_native(current) == mode {
        PlanChange { mode: current, changed: false, persistence: PlanPersistence::Unchanged }
    } else {
        match mode {
            Mode::Plan => handle.enter_plan_mode(cx).await,
            Mode::Code => handle.exit_plan_mode(cx).await,
        }.map_err(|error| failure(INVALID_PARAMS, &error.to_string()))?
    };
    // Native transitions are live before saving. Never report a failed save as
    // a rollback or allow a provider to act on an undurable approval.
    let persistence = confirm_persistence(handle, &change.persistence);
    out.send(mode_update(session_id, change.mode))
        .map_err(|_| failure(INTERNAL_ERROR, "ACP output is unavailable"))?;
    let configuration = config_options_for_handle(handle, &available_models, &config, &cwd)
        .map_err(|_| failure(INTERNAL_ERROR, "Session mode changed but configuration is unavailable"))?;
    out.send(config_options_update(session_id, &configuration))
        .map_err(|_| failure(INTERNAL_ERROR, "ACP output is unavailable"))?;
    if persistence.is_err() {
        return Err(JsonRpcError {
            code: INTERNAL_ERROR,
            message: "Session mode changed in memory, but saving failed. Further prompts are blocked; resume the saved session.".into(),
            data: Some(json!({
                "applied": change.changed,
                "currentModeId": Mode::from_native(change.mode).id(),
                "planPersistence": "unconfirmed",
            })),
        });
    }
    Ok(configuration)
}

fn confirm_persistence(handle: &AgentSessionHandle, persistence: &PlanPersistence) -> Result<()> {
    if let PlanPersistence::Unconfirmed { reason } = persistence {
        handle.session().provider_admission_gate().block(format!("ACP plan persistence is unconfirmed: {reason}"));
        return Err(Error::session_persistence("ACP plan transition was not saved"));
    }
    Ok(())
}

/// Restoring never imports an execution approval. SDK checkpoint recovery
/// demotes approved plans to a fresh, session-owned pending review.
pub(super) async fn restore(state: &mut AcpSessionState, cx: &AgentCx) -> Result<()> {
    let Some(handle) = state.agent_session.as_mut() else {
        return Err(Error::session("Cannot restore planning without the session owner"));
    };
    let replayed = {
        let store = handle.session_store();
        let session = store.lock(cx).await?;
        crate::plan::replayed_plan_mode(&session)
    };
    if !enabled(handle) {
        if replayed != PlanMode::Off {
            return Err(Error::validation("Cannot restore a planning session while built-in tools are disabled"));
        }
        return Ok(());
    }
    let change = match handle.restore_plan_checkpoint(cx).await {
        Ok(change) => change,
        Err(Error::Validation(message)) if message.starts_with("[PLAN_CHECKPOINT_MISSING]") => {
            if replayed == PlanMode::Off {
                return Ok(());
            }
            // Old sessions may contain mode journals without recoverable plan
            // text. Preserve the read-only gate and require a fresh proposal.
            handle.enter_plan_mode(cx).await?
        }
        Err(error) => return Err(error),
    };
    confirm_persistence(handle, &change.persistence)
}

/// Native PlanState owns authorization; this transient block tells the model
/// what the editor selected. Remove it before approving so the SDK's owned plan
/// pin does not retain obsolete read-only instructions during implementation.
pub(super) struct PromptGuidance {
    block: String,
    prior_was_none: bool,
}

impl PromptGuidance {
    pub(super) fn install(handle: &mut AgentSessionHandle) -> Option<Self> {
        let mode = handle.session().agent.plan_state().mode();
        let instruction = match mode {
            PlanMode::Planning => "Investigate with read, search and analysis tools only. Do not create, edit or delete files or run processes. Prepare a concrete plan with the goal, ordered steps, files to change and verification. Call submit_plan with the complete proposal, then finish your turn and await the editor's review. Implementation requires the user's approval of that exact proposal.",
            PlanMode::PendingApproval => "A submitted proposal is awaiting the editor's review. Keep all work read-only. Do not submit a replacement or implement the proposal. Finish your turn so the editor can review the existing plan.",
            PlanMode::Off | PlanMode::Approved => return None,
        };
        let marker = uuid::Uuid::new_v4();
        let block = format!(
            "\n\n<!-- pi-acp-plan-mode:{marker} -->\nACP session mode: Plan.\n{instruction}\n<!-- /pi-acp-plan-mode:{marker} -->"
        );
        let prior = handle.session().agent.system_prompt();
        let prior_was_none = prior.is_none();
        let prompt = format!("{}{}", prior.unwrap_or_default(), block);
        handle.session_mut().agent.set_system_prompt(Some(prompt));
        Some(Self { block, prior_was_none })
    }

    pub(super) fn remove(self, handle: &mut AgentSessionHandle) {
        let current = handle.session().agent.system_prompt();
        let Some(current) = current else { return };
        if !current.contains(&self.block) {
            return;
        }
        // Match only the uniquely owned block, preserving unrelated prompt
        // additions made by native extensions while the turn was running.
        let prompt = current.replace(&self.block, "");
        handle.session_mut().agent.set_system_prompt(
            if prompt.is_empty() && self.prior_was_none { None } else { Some(prompt) }
        );
    }
}

fn review_request(request_id: &Value, session_id: &str, text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": request_id,
                "title": "Review plan before implementation",
                "kind": "switch_mode",
                "status": "pending",
                "content": [{"type": "content", "content": {"type": "text", "text": text}}],
            },
            "options": [
                {"optionId": ACP_PERMISSION_ALLOW_ONCE, "name": "Approve plan and implement", "kind": "allow_once"},
                {"optionId": ACP_PERMISSION_REJECT_ONCE, "name": "Reject plan and keep planning", "kind": "reject_once"}
            ],
        }
    }).to_string()
}

/// Bridge one native submission into the editor's permission dialog. The exact
/// review stays owned across the await; an old answer cannot approve new text.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_turn(
    handle: &mut AgentSessionHandle,
    initial: Result<AssistantMessage>,
    abort: AbortSignal,
    cx: &AgentCx,
    out: &AcpOutput,
    session_id: &str,
    updates: &AcpRecoveryUpdates,
    on_event: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> Result<AssistantMessage> {
    let message = initial?;
    if !matches!(message.stop_reason, StopReason::Stop | StopReason::ToolUse | StopReason::PauseTurn) {
        return Ok(message);
    }
    match finish_command(handle, abort, cx, out, session_id, updates, on_event).await {
        Some(result) => result,
        None => Ok(message),
    }
}

/// Extension commands can submit plans through their generated native turns.
/// Run the same owned review after the command's persistence boundary.
pub(super) async fn finish_command(
    handle: &mut AgentSessionHandle,
    abort: AbortSignal,
    cx: &AgentCx,
    out: &AcpOutput,
    session_id: &str,
    updates: &AcpRecoveryUpdates,
    on_event: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> Option<Result<AssistantMessage>> {
    match review_pending(handle, &abort, cx, out, session_id, updates).await {
        Ok(true) => Some(handle.continue_turn_with_abort(abort, move |event| on_event(event)).await),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
    }
}

async fn review_pending(
    handle: &mut AgentSessionHandle,
    abort: &AbortSignal,
    cx: &AgentCx,
    out: &AcpOutput,
    session_id: &str,
    updates: &AcpRecoveryUpdates,
) -> Result<bool> {
    if abort.is_aborted() || cx.is_cancel_requested() {
        return Err(Error::Aborted);
    }
    handle.session().ensure_provider_reentry_allowed()?;
    // Publish mode changes caused by native prompt/extension actions before
    // presenting review, including legacy clients that do not use configOptions.
    updates.finish(handle);
    let Some(review) = handle.pending_plan_review()? else {
        return Ok(false);
    };
    let request_id = json!(format!("pi-plan-permission-{}", uuid::Uuid::new_v4()));
    let key = json_rpc_id_key(&request_id);
    let (sender, mut receiver) = oneshot::channel();
    if out.register_permission(
        key.clone(), session_id, request_id.clone(), sender,
        review_request(&request_id, session_id, review.text()),
    ).is_err() {
        return if abort.is_aborted() { Err(Error::Aborted) } else { Ok(false) };
    }
    let pending = PendingPermissionGuard {
        pending: Arc::clone(&out.shared.pending_permissions),
        key,
    };
    let response = timeout(wall_now(), acp_permission_timeout(), Box::pin(receiver.recv(cx.cx()))).await;
    drop(pending);
    if abort.is_aborted() || cx.is_cancel_requested() {
        return Err(Error::Aborted);
    }
    if !out.mode_change_admitted()
        || out.prompt_input(session_id).is_some_and(|input| !input.permits_tool())
    {
        return Ok(false);
    }
    let response = match response {
        Ok(Ok(value)) => value,
        _ => {
            let _ = out.send(json_rpc_notification("$/cancel_request", json!({"requestId": request_id})));
            return Ok(false);
        }
    };
    let approved = matches!(permission_response_to_decision(&response), ToolApprovalDecision::Allow);
    let rejected = response.get("error").is_none()
        && response["outcome"]["outcome"] == "selected"
        && response["outcome"]["optionId"] == ACP_PERMISSION_REJECT_ONCE;
    if !approved && !rejected {
        return Ok(false);
    }
    let change = if approved {
        handle.approve_plan_review(cx, &review).await?
    } else {
        handle.reject_plan_review(cx, &review).await?
    };
    let persistence = confirm_persistence(handle, &change.persistence);
    updates.finish(handle);
    persistence?;
    // The caller continues the approved conversation without fabricating a
    // user message. Retry, cancellation, budgets and saving remain SDK-owned.
    Ok(approved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_reservation_covers_the_interval_after_an_old_prompt_retires() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(8);
        let output = AcpOutput::new(sender);
        let (abort, _) = AbortHandle::new();
        let prompt = output.begin_prompt("session".into(), abort).unwrap();
        let mode = output.reserve_mode_change("session").unwrap();
        assert!(output.reserve_mode_change("session").is_err());
        drop(prompt);
        assert!(!output.prompt_is_active("session"));
        assert!(output.session_is_busy("session"));
        let (abort, _) = AbortHandle::new();
        assert!(output.begin_prompt("session".into(), abort).is_err());
        drop(mode);
        assert!(!output.session_is_busy("session"));
        let (abort, _) = AbortHandle::new();
        let _next = output.begin_prompt("session".into(), abort).unwrap();
    }
}
