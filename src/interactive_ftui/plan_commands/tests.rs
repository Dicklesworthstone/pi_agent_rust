use super::*;
use crate::approval::{ApprovalMode, ApprovalState};
use crate::session::Session;
use crate::tools::{Tool, ToolEffects};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const PLAN: &str = "Goal: improve the parser. Steps: inspect code, edit it, and verify behavior.";

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .build()
        .unwrap()
        .block_on(future)
}

// The actual SDK and native provider, not a controller-side imitation of plan
// state. The unroutable endpoint catches accidental provider use; listeners
// independently check that these controls never initiate an agent turn.
fn fixture(stored: Session) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    fixture_with_persistence(stored, false)
}

fn fixture_with_persistence(stored: Session, save: bool) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let provider = Arc::new(
        crate::providers::openai::OpenAIProvider::new("ftui-plan-control-fixture")
            .with_base_url("http://127.0.0.1:1/v1"),
    );
    let agent = crate::agent::Agent::new(
        provider,
        crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None),
        crate::agent::AgentConfig {
            system_prompt: Some("original instructions".to_string()),
            approval_state: Some(ApprovalState::new(
                ApprovalMode::AlwaysAsk,
                false,
                Vec::new(),
            )),
            ..crate::agent::AgentConfig::default()
        },
    );
    let session = crate::agent::AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(stored)),
        save,
        crate::compaction::ResolvedCompactionSettings::default(),
    );
    let events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&events);
    let listeners = crate::sdk::EventListeners::default();
    let _ = listeners.subscribe(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    (
        AgentSessionHandle::from_session_with_listeners(session, listeners),
        events,
    )
}

fn submit(handle: &AgentSessionHandle, text: &str) {
    submit_input(
        handle,
        json!({"plan": text, "files": ["src/hello world.rs"]}),
    );
}

/// Submit without a `files` scope. A plan that leaves a code fence open
/// cannot also carry an appended `Files:` declaration (the scope check
/// rejects it as ambiguous), so hostile-display plans go through here.
fn submit_unscoped(handle: &AgentSessionHandle, text: &str) {
    submit_input(handle, json!({"plan": text}));
}

fn submit_input(handle: &AgentSessionHandle, input: serde_json::Value) {
    let state = handle.session().agent.plan_state();
    let tool = crate::plan::SubmitPlanTool::new(state, false);
    let output = run(tool.execute("proposal", input, None)).unwrap();
    assert!(
        !output.is_error,
        "submit_plan refused the fixture: {output:?}"
    );
}

fn pending() -> (AgentSessionHandle, PlanController, Arc<AtomicUsize>) {
    let (mut handle, events) = fixture(Session::in_memory());
    let mut controller = PlanController::default();
    run(controller.execute(&mut handle, "enter")).unwrap();
    submit(&handle, PLAN);
    (handle, controller, events)
}

fn display(controller: &mut PlanController, handle: &mut AgentSessionHandle) -> Uuid {
    let output = run(controller.execute(handle, "review")).unwrap();
    let id = controller.displayed.as_ref().unwrap().id;
    assert!(output.contains(&format!("Review ID: {id}")));
    id
}

#[test]
fn command_admission_is_exact_and_case_insensitive() {
    for (args, expected) in [
        ("", PlanCommand::Enter),
        ("  \n", PlanCommand::Enter),
        ("EnTeR", PlanCommand::Enter),
        ("status", PlanCommand::Status),
        (" ReViEw ", PlanCommand::Review),
        ("off", PlanCommand::Exit),
        ("exit", PlanCommand::Exit),
        ("restore", PlanCommand::Restore),
        ("save", PlanCommand::Save),
    ] {
        assert_eq!(parse(args).unwrap(), expected);
    }
    let id = Uuid::new_v4();
    assert_eq!(
        parse(&format!("ApPrOvE {id}")).unwrap(),
        PlanCommand::Approve(id)
    );
    assert_eq!(
        parse(&format!("REJECT {id}")).unwrap(),
        PlanCommand::Reject(id)
    );
}

#[test]
fn approval_without_display_id_and_trailing_arguments_are_rejected() {
    let id = Uuid::new_v4();
    for args in [
        "approve".to_string(),
        "reject".to_string(),
        "approve yes".to_string(),
        "review now".to_string(),
        "off now".to_string(),
        "restore now".to_string(),
        "save now".to_string(),
        "status review".to_string(),
        "enter do work".to_string(),
        "approved".to_string(),
        format!("approve {id} extra"),
        format!("reject {id} extra"),
    ] {
        assert!(parse(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn default_ftui_routes_plan_and_preserves_extension_prefixes() {
    use super::super::{PiFtuiModel, UiCommand};
    let (_agent_tx, agent_rx) = std::sync::mpsc::channel();
    let (submit_tx, submit_rx) = std::sync::mpsc::channel();
    let mut model = PiFtuiModel::new(agent_rx).with_submit_channel(submit_tx);
    assert!(model.route_slash_command("/PlAn review"));
    assert_eq!(
        submit_rx.try_recv().unwrap(),
        UiCommand::Plan {
            action: "review".to_string()
        },
    );
    assert!(model.route_slash_command("/plan approve"));
    assert!(submit_rx.try_recv().is_err());
    assert!(model.route_slash_command("/planet status"));
    assert_eq!(
        submit_rx.try_recv().unwrap(),
        UiCommand::ExtensionCommand {
            name: "planet".to_string(),
            args: "status".to_string()
        },
    );
}

#[test]
fn workflow_uses_native_review_pins_context_and_never_starts_a_turn() {
    let (mut handle, mut controller, events) = pending();
    let state = handle.session().agent.plan_state();
    assert!(!state.allows_effects(ToolEffects::write()));
    assert!(!state.allows_effects(ToolEffects::process()));
    let id = display(&mut controller, &mut handle);
    let reviewed = controller
        .displayed
        .as_ref()
        .unwrap()
        .review
        .text()
        .to_string();
    let output = run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap();
    assert!(output.contains("No execution turn was started"));
    assert!(output.contains("Memory-only"));
    assert_eq!(state.mode(), PlanMode::Approved);
    assert!(
        handle
            .session()
            .agent
            .system_prompt()
            .unwrap()
            .contains(&reviewed)
    );
    let policy = handle.session().agent.approval_state().unwrap();
    assert_eq!(policy.mode(), ApprovalMode::AlwaysAsk);
    assert!(
        !policy
            .evaluate(
                "write",
                &json!({"path": "src/hello world.rs"}),
                ToolEffects::write(),
                Some(&state),
                None,
            )
            .is_auto_approved()
    );
    assert!(controller.displayed.is_none());
    assert!(run(controller.execute(&mut handle, &format!("approve {id}"))).is_err());
    run(controller.execute(&mut handle, "off")).unwrap();
    assert_eq!(state.mode(), PlanMode::Off);
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn status_is_not_review_and_cannot_authorize_approval() {
    let (mut handle, mut controller, _) = pending();
    let output = run(controller.execute(&mut handle, "status")).unwrap();
    assert!(output.contains("pending_approval"));
    assert!(!output.contains(PLAN));
    assert!(controller.displayed.is_none());
    assert!(run(controller.execute(&mut handle, &format!("approve {}", Uuid::new_v4()))).is_err());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn redisplay_invalidates_the_old_display_id_even_for_the_same_proposal() {
    let (mut handle, mut controller, _) = pending();
    let old = display(&mut controller, &mut handle);
    let current = display(&mut controller, &mut handle);
    assert_ne!(old, current);
    assert!(run(controller.execute(&mut handle, &format!("approve {old}"))).is_err());
    assert_eq!(controller.displayed.as_ref().unwrap().id, current);
    run(controller.execute(&mut handle, &format!("approve {current}"))).unwrap();
}

#[test]
fn rejection_returns_to_planning_and_requires_a_new_review() {
    let (mut handle, mut controller, events) = pending();
    let old = display(&mut controller, &mut handle);
    run(controller.execute(&mut handle, &format!("reject {old}"))).unwrap();
    let state = handle.session().agent.plan_state();
    assert_eq!(state.mode(), PlanMode::Planning);
    assert!(!state.allows_effects(ToolEffects::write()));
    submit(&handle, PLAN);
    assert!(run(controller.execute(&mut handle, &format!("approve {old}"))).is_err());
    let current = display(&mut controller, &mut handle);
    run(controller.execute(&mut handle, &format!("approve {current}"))).unwrap();
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn identical_external_resubmission_cannot_retarget_a_displayed_decision() {
    for verb in ["approve", "reject"] {
        let (mut handle, mut controller, _) = pending();
        let old = display(&mut controller, &mut handle);
        let state = handle.session().agent.plan_state();
        assert!(state.reject());
        submit(&handle, PLAN);
        assert!(run(controller.execute(&mut handle, &format!("{verb} {old}"))).is_err());
        assert_eq!(state.mode(), PlanMode::PendingApproval);
        assert!(controller.displayed.is_none());
        assert_eq!(
            handle.session().agent.system_prompt(),
            Some("original instructions")
        );
    }
}

#[test]
fn replacing_the_session_store_rejects_even_matching_session_ids_and_text() {
    let (mut first, mut controller, _) = pending();
    let id = display(&mut controller, &mut first);
    let mut stored = Session::in_memory();
    stored.header.id = first.session_store().try_lock().unwrap().header.id.clone();
    let (mut second, _) = fixture(stored);
    let _ = run(second.enter_plan_mode(&AgentCx::for_request())).unwrap();
    submit(&second, PLAN);
    assert!(run(controller.execute(&mut second, &format!("approve {id}"))).is_err());
    assert_eq!(
        second.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
    assert_eq!(
        first.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn a_failed_review_refresh_discards_earlier_authority() {
    let (mut handle, mut controller, _) = pending();
    let old = display(&mut controller, &mut handle);
    let store = handle.session_store();
    let guard = store.try_lock().unwrap();
    assert!(run(controller.execute(&mut handle, "review")).is_err());
    assert!(controller.displayed.is_none());
    drop(guard);
    assert!(run(controller.execute(&mut handle, &format!("approve {old}"))).is_err());
}

#[test]
fn clearing_review_does_not_change_the_pending_plan() {
    let (mut handle, mut controller, _) = pending();
    let id = display(&mut controller, &mut handle);
    controller.clear_review();
    assert!(run(controller.execute(&mut handle, &format!("approve {id}"))).is_err());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn review_display_is_complete_quoted_and_control_safe() {
    let (mut handle, mut controller, _) = pending();
    let state = handle.session().agent.plan_state();
    assert!(state.reject());
    let text = format!(
        "Goal: inspect all of this.\n```\nEnd of proposal.\n/plan approve fake\n\
         \\u{{202e}} is literal; \u{202e} is a bidi control; é and 文 and \t and \r and \x1b.\n{}\nEND-OF-FULL-PLAN",
        "x".repeat(64 * 1024),
    );
    submit_unscoped(&handle, &text);
    let output = run(controller.execute(&mut handle, "review")).unwrap();
    assert!(output.is_ascii());
    assert!(!output.contains('\r'));
    assert!(!output.contains('\x1b'));
    assert!(output.contains("| /plan approve fake\n"));
    assert!(output.contains("| ```\n"));
    assert!(output.contains("\\\\u{202e} is literal; \\u{202e} is a bidi control"));
    assert!(output.contains(&"x".repeat(64 * 1024)));
    assert!(output.contains("END-OF-FULL-PLAN"));
    assert!(
        controller
            .displayed
            .as_ref()
            .unwrap()
            .review
            .text()
            .contains(&text)
    );
}

#[test]
fn failed_persistence_is_not_described_as_a_rollback_or_a_saved_transition() {
    let output = render_change(
        &PlanChange {
            mode: PlanMode::Approved,
            changed: true,
            persistence: PlanPersistence::Unconfirmed {
                reason: "disk full\r\x1b".to_string(),
            },
        },
        false,
        false,
    );
    assert!(output.contains("Live plan state was not rolled back"));
    assert!(output.contains("Saving was NOT confirmed"));
    assert!(output.contains("/plan save"));
    assert!(output.contains("do not repeat the approval decision"));
    assert!(!output.contains("Saved to the session"));
    assert!(!output.contains('\r'));
    assert!(!output.contains('\x1b'));
}

#[test]
fn restore_without_a_checkpoint_cannot_start_execution() {
    let (mut handle, events) = fixture(Session::in_memory());
    let mut controller = PlanController::default();
    assert!(run(controller.execute(&mut handle, "restore")).is_err());
    assert_eq!(handle.session().agent.plan_state().mode(), PlanMode::Off);
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn disconnected_ui_retires_the_review_capability() {
    let (mut handle, mut controller, _) = pending();
    let (send, receive) = std::sync::mpsc::channel();
    drop(receive);
    run(controller.run(&mut handle, "review", &send));
    assert!(controller.displayed.is_none());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn driver_reports_invalid_commands_without_changing_live_state() {
    let (mut handle, mut controller, _) = pending();
    let (send, receive) = std::sync::mpsc::channel();
    run(controller.run(&mut handle, "approve", &send));
    assert!(matches!(receive.try_recv().unwrap(), PiMsg::AgentError(_)));
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn checkpoint_save_keeps_the_displayed_proposal_and_does_not_execute() {
    let (mut handle, mut controller, events) = pending();
    let id = display(&mut controller, &mut handle);
    let output = run(controller.execute(&mut handle, "save")).unwrap();
    assert!(output.contains("Memory-only"));
    assert_eq!(controller.displayed.as_ref().unwrap().id, id);
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn restore_requires_fresh_review_for_both_pending_and_approved_checkpoints() {
    for approve_before_reopen in [false, true] {
        let (mut original, mut controller, _) = pending();
        let old_id = display(&mut controller, &mut original);
        run(controller.execute(&mut original, "save")).unwrap();
        if approve_before_reopen {
            run(controller.execute(&mut original, &format!("approve {old_id}"))).unwrap();
        }
        // Move the complete journal into a new SDK handle. This exercises
        // checkpoint recovery across actual store/agent incarnations, without
        // claiming to exercise disk serialization in this frontend test.
        let store = original.session_store();
        let stored = std::mem::replace(&mut *store.try_lock().unwrap(), Session::in_memory());
        let (mut reopened, events) = fixture(stored);
        let output = run(controller.execute(&mut reopened, "restore")).unwrap();
        assert!(output.contains("pending_approval"));
        assert!(controller.displayed.is_none());
        assert!(
            !reopened
                .session()
                .agent
                .plan_state()
                .allows_effects(ToolEffects::write())
        );
        assert_eq!(
            reopened.session().agent.system_prompt(),
            Some("original instructions")
        );
        assert!(run(controller.execute(&mut reopened, &format!("approve {old_id}"))).is_err());
        let new_id = display(&mut controller, &mut reopened);
        assert_ne!(new_id, old_id);
        run(controller.execute(&mut reopened, &format!("approve {new_id}"))).unwrap();
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}

fn role_entries() -> Vec<crate::models::ModelEntry> {
    use crate::provider::{InputType, Model, ModelCost};
    [
        (
            "openai",
            "ftui-plan-control-fixture",
            "openai-completions",
            true,
            8192,
            1024,
        ),
        (
            "anthropic",
            "plan-role-fixture",
            "anthropic-messages",
            false,
            65_536,
            4096,
        ),
        (
            "ollama",
            "implementation-fixture",
            "openai-completions",
            true,
            16_384,
            2048,
        ),
    ]
    .into_iter()
    .map(
        |(provider, id, api, images, context_window, max_tokens)| crate::models::ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: api.to_string(),
                provider: provider.to_string(),
                base_url: "http://127.0.0.1:1/v1".to_string(),
                reasoning: true,
                input: if images {
                    vec![InputType::Text, InputType::Image]
                } else {
                    vec![InputType::Text]
                },
                cost: ModelCost {
                    audio: None,
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window,
                max_tokens,
                headers: std::collections::HashMap::new(),
            },
            api_key: Some(format!("{provider}-fixture-key")),
            headers: std::collections::HashMap::from([(
                "x-fixture-provider".to_string(),
                provider.to_string(),
            )]),
            auth_header: true,
            compat: None,
            oauth_config: None,
        },
    )
    .collect()
}

fn role_fixture(stored: Session, save: bool) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let (mut handle, events) = fixture_with_persistence(stored, save);
    handle
        .session_mut()
        .set_model_registry(crate::models::ModelRegistry::from_entries_for_tests(
            role_entries(),
        ));
    run(handle.set_model("openai", "ftui-plan-control-fixture")).unwrap();
    run(handle.set_thinking_level(ThinkingLevel::High)).unwrap();
    (handle, events)
}

fn role_controller() -> PlanController {
    PlanController::new(Some("anthropic/plan-role-fixture:low".to_string()))
}

#[test]
fn plan_role_restores_native_model_reasoning_credentials_and_capabilities() {
    for approve in [false, true] {
        let (mut handle, events) = role_fixture(Session::in_memory(), false);
        let original = ModelSelection::current(&handle);
        let mut controller = role_controller();
        let entered = run(controller.execute(&mut handle, "enter")).unwrap();
        assert!(entered.contains("Planning model: anthropic/plan-role-fixture:low"));
        assert_eq!(
            handle.model(),
            ("anthropic".into(), "plan-role-fixture".into())
        );
        assert_eq!(handle.thinking_level(), Some(ThinkingLevel::Low));
        let options = handle.session().agent.stream_options();
        assert_eq!(options.api_key.as_deref(), Some("anthropic-fixture-key"));
        assert_eq!(options.headers["x-fixture-provider"], "anthropic");
        assert_eq!(options.max_tokens, Some(4096));
        assert_eq!(handle.compaction_settings().context_window_tokens, 65_536);
        assert!(!handle.session().agent.model_accepts_images());
        assert!(
            !handle
                .session()
                .agent
                .plan_state()
                .allows_effects(ToolEffects::write())
        );

        // Re-entering cannot overwrite the execution model with the planner.
        run(controller.execute(&mut handle, "enter")).unwrap();
        let output = if approve {
            submit(&handle, PLAN);
            let rejected = display(&mut controller, &mut handle);
            run(controller.execute(&mut handle, &format!("reject {rejected}"))).unwrap();
            assert_eq!(handle.model().0, "anthropic");
            submit(&handle, PLAN);
            let id = display(&mut controller, &mut handle);
            run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap()
        } else {
            run(controller.execute(&mut handle, "off")).unwrap()
        };
        assert!(output.contains("Restored execution model: openai/ftui-plan-control-fixture:high"));
        assert_eq!(ModelSelection::current(&handle), original);
        let options = handle.session().agent.stream_options();
        assert_eq!(options.api_key.as_deref(), Some("openai-fixture-key"));
        assert_eq!(options.headers["x-fixture-provider"], "openai");
        assert_eq!(options.max_tokens, Some(1024));
        assert_eq!(handle.compaction_settings().context_window_tokens, 8192);
        assert!(handle.session().agent.model_accepts_images());
        assert!(controller.model_restore.is_none());
        assert_eq!(
            events.load(Ordering::SeqCst),
            0,
            "role controls must not issue a turn"
        );
    }
}

#[test]
fn planning_failover_retains_execution_model_restoration_until_approval_or_exit() {
    for approve in [false, true] {
        let (mut handle, events) = role_fixture(Session::in_memory(), false);
        let mut controller = role_controller();
        run(controller.execute(&mut handle, "enter")).unwrap();
        let fallback = role_entries().pop().unwrap();
        let primary = crate::failover::FailoverPrimary {
            provider: "anthropic".to_string(),
            model_id: "plan-role-fixture".to_string(),
            requested_thinking_level: ThinkingLevel::Low,
        };
        let request = crate::agent::FailoverSwapRequest {
            entry: &fallback,
            api_key: fallback.api_key.clone(),
            provider: crate::providers::create_provider_with_auth(&fallback, None, None).unwrap(),
            from_provider: "anthropic",
            from_model: "plan-role-fixture",
            class: crate::failover::FailoverClass::Quota,
            chain_position: 1,
            thinking_level_to_clamp: ThinkingLevel::Low,
            require_incomplete_tail: false,
            primary: Some(&primary),
            cooldown_secs: Some(300),
            lifecycle_id: Some("planning-failover-fixture"),
        };
        run(handle
            .session_mut()
            .commit_failover(&AgentCx::for_current_or_request(), &request))
        .unwrap();
        assert_eq!(handle.model().0, "ollama");
        let output = if approve {
            submit(&handle, PLAN);
            let id = display(&mut controller, &mut handle);
            run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap()
        } else {
            run(controller.execute(&mut handle, "off")).unwrap()
        };
        assert!(output.contains("Restored execution model"));
        assert_eq!(handle.model().0, "openai");
        assert_eq!(handle.thinking_level(), Some(ThinkingLevel::High));
        assert!(
            handle
                .session_store()
                .try_lock()
                .unwrap()
                .active_failover_provenance_for_current_path()
                .is_none()
        );
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn automatic_plan_selection_preserves_prewalk_but_manual_selection_disarms_it() {
    let (mut handle, _) = role_fixture(Session::in_memory(), false);
    let target = run(handle.arm_prewalk(crate::prewalk::PrewalkTarget {
        provider: "ollama".to_string(),
        model: "implementation-fixture".to_string(),
        thinking: Some(ThinkingLevel::Medium),
    }))
    .unwrap()
    .unwrap();
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    assert_eq!(handle.prewalk_target(), Some(&target));
    submit(&handle, PLAN);
    let id = display(&mut controller, &mut handle);
    run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap();
    assert_eq!(handle.prewalk_target(), Some(&target));
    run(handle.set_model("openai", "ftui-plan-control-fixture")).unwrap();
    assert!(handle.prewalk_target().is_none());
}

#[test]
fn cli_execution_key_stays_with_its_provider_and_returns_after_planning() {
    let (mut handle, _) = role_fixture(Session::in_memory(), false);
    handle
        .session_mut()
        .set_api_key_override(Some("execution-only-cli-key".to_string()));
    let mut entries = role_entries();
    entries[0].api_key = None;
    let mut same_provider = entries[0].clone();
    same_provider.model.id = "second-execution-model".to_string();
    entries.push(same_provider);
    handle
        .session_mut()
        .set_model_registry(crate::models::ModelRegistry::from_entries_for_tests(
            entries,
        ));
    run(handle.set_model("openai", "ftui-plan-control-fixture")).unwrap();
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    assert_eq!(
        handle.session().agent.stream_options().api_key.as_deref(),
        Some("anthropic-fixture-key"),
    );
    // Logging in/out while planning must not reapply the execution pin.
    let temp = tempfile::tempdir().unwrap();
    handle.adopt_auth_storage(crate::auth::AuthStorage::empty_at(
        temp.path().join("auth.json"),
    ));
    assert_eq!(
        handle.session().agent.stream_options().api_key.as_deref(),
        Some("anthropic-fixture-key"),
    );
    run(controller.execute(&mut handle, "off")).unwrap();
    assert_eq!(
        handle.session().agent.stream_options().api_key.as_deref(),
        Some("execution-only-cli-key"),
    );
    run(handle.set_model("openai", "second-execution-model")).unwrap();
    assert_eq!(
        handle.session().agent.stream_options().api_key.as_deref(),
        Some("execution-only-cli-key"),
        "models belonging to the same provider share its explicit key",
    );
}

#[test]
fn explicit_same_model_or_thinking_selection_retires_automatic_restore() {
    for thinking in [false, true] {
        let (mut handle, _) = role_fixture(Session::in_memory(), false);
        let mut controller = role_controller();
        run(controller.execute(&mut handle, "enter")).unwrap();
        let (send, _receive) = std::sync::mpsc::channel();
        let selected = if thinking {
            run(super::super::run_set_thinking_command(
                &mut handle,
                Some(ThinkingLevel::Low),
                &send,
            ))
        } else {
            run(super::super::run_set_model_command(
                &mut handle,
                "anthropic",
                "plan-role-fixture",
                &send,
            ))
        };
        assert!(
            selected,
            "even reselecting the active value is explicit intent"
        );
        controller.forget_model_restore();
        let output = run(controller.execute(&mut handle, "off")).unwrap();
        assert_eq!(handle.model().0, "anthropic");
        assert_eq!(handle.thinking_level(), Some(ThinkingLevel::Low));
        assert!(!output.contains("Restored execution model"));
    }
}

#[test]
fn failed_manual_selection_and_thinking_query_do_not_retire_restore() {
    let (mut handle, _) = role_fixture(Session::in_memory(), false);
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    let (send, _receive) = std::sync::mpsc::channel();
    assert!(!run(super::super::run_set_model_command(
        &mut handle,
        "anthropic",
        "missing-model",
        &send,
    )));
    assert!(!run(super::super::run_set_thinking_command(
        &mut handle,
        None,
        &send
    )));
    run(controller.execute(&mut handle, "off")).unwrap();
    assert_eq!(handle.model().0, "openai");
    assert_eq!(handle.thinking_level(), Some(ThinkingLevel::High));
}

#[test]
fn a_newer_native_selection_wins_even_without_a_terminal_command() {
    let (mut handle, _) = role_fixture(Session::in_memory(), false);
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    run(handle.set_model("ollama", "implementation-fixture")).unwrap();
    run(controller.execute(&mut handle, "off")).unwrap();
    assert_eq!(handle.model().0, "ollama");
    assert!(controller.model_restore.is_none());
}

#[test]
fn a_replaced_session_never_receives_the_previous_sessions_restore() {
    let (mut first, _) = role_fixture(Session::in_memory(), false);
    let mut controller = role_controller();
    run(controller.execute(&mut first, "enter")).unwrap();
    let mut stored = Session::in_memory();
    stored.header.id = first.session_store().try_lock().unwrap().header.id.clone();
    let (mut second, _) = role_fixture(stored, false);
    run(second.set_model("anthropic", "plan-role-fixture")).unwrap();
    run(second.set_thinking_level(ThinkingLevel::Low)).unwrap();
    run(controller.execute(&mut second, "off")).unwrap();
    assert_eq!(second.model().0, "anthropic");
    assert!(controller.model_restore.is_none());
}

#[test]
fn an_invalid_plan_role_never_picks_an_unrelated_catalog_model() {
    let (mut handle, events) = role_fixture(Session::in_memory(), false);
    let original = ModelSelection::current(&handle);
    let mut controller = PlanController::new(Some("anthropic/not-registered".to_string()));
    let output = run(controller.execute(&mut handle, "enter")).unwrap();
    assert!(output.contains("plan model could not be selected"));
    assert_eq!(ModelSelection::current(&handle), original);
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::Planning
    );
    assert!(
        !handle
            .session()
            .agent
            .plan_state()
            .allows_effects(ToolEffects::write())
    );
    assert!(controller.model_restore.is_none());
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_restoration_is_visible_and_plan_save_can_retry_it_without_reapproval() {
    let (mut handle, _) = role_fixture(Session::in_memory(), false);
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    let mut entries = role_entries();
    entries[0].api_key = None;
    handle
        .session_mut()
        .set_model_registry(crate::models::ModelRegistry::from_entries_for_tests(
            entries,
        ));
    submit(&handle, PLAN);
    let id = display(&mut controller, &mut handle);
    let output = run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap();
    assert!(output.contains("restoration was NOT confirmed"));
    assert!(!output.contains("Send a prompt to begin execution"));
    let status = run(controller.execute(&mut handle, "status")).unwrap();
    assert!(status.contains("Execution model restoration is pending"));
    assert!(!status.contains("Send a prompt to begin execution"));
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::Approved
    );
    assert_eq!(handle.model().0, "anthropic");
    assert!(controller.model_restore.is_some());
    assert!(controller.displayed.is_none());
    handle
        .session_mut()
        .set_model_registry(crate::models::ModelRegistry::from_entries_for_tests(
            role_entries(),
        ));
    let output = run(controller.execute(&mut handle, "save")).unwrap();
    assert!(output.contains("Restored execution model"));
    assert_eq!(handle.model().0, "openai");
    assert_eq!(handle.thinking_level(), Some(ThinkingLevel::High));
}

#[test]
fn unconfirmed_plan_persistence_defers_selection_and_does_not_advertise_execution() {
    let temp = tempfile::tempdir().unwrap();
    let mut stored = Session::in_memory();
    stored.path = Some(temp.path().join("session.jsonl"));
    let (mut handle, events) = role_fixture(stored, true);
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, b"sentinel").unwrap();
    handle.session_store().try_lock().unwrap().path = Some(blocker.join("session.jsonl"));
    let original = ModelSelection::current(&handle);
    let mut controller = role_controller();
    let output = run(controller.execute(&mut handle, "enter")).unwrap();
    assert!(output.contains("Persistence is unconfirmed"));
    assert_eq!(ModelSelection::current(&handle), original);
    submit(&handle, PLAN);
    let id = display(&mut controller, &mut handle);
    let output = run(controller.execute(&mut handle, &format!("approve {id}"))).unwrap();
    assert!(output.contains("Persistence is unconfirmed"));
    assert!(!output.contains("Send a prompt to begin execution"));
    assert_eq!(ModelSelection::current(&handle), original);
    assert_eq!(events.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&blocker).unwrap(), b"sentinel");
}

#[test]
fn indeterminate_model_save_requires_session_recovery_not_a_plan_save() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    let mut stored = Session::in_memory();
    stored.path = Some(path.clone());
    let (mut handle, events) = role_fixture(stored, true);
    let mut controller = role_controller();
    run(controller.execute(&mut handle, "enter")).unwrap();
    let origin = handle
        .session()
        .session_action_admission_gate()
        .capture_origin();
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, b"sentinel").unwrap();
    handle.session_store().try_lock().unwrap().path = Some(blocker.join("session.jsonl"));
    let error = run(handle.set_plan_role_model(
        "openai",
        "ftui-plan-control-fixture",
        ThinkingLevel::High,
        &origin,
    ))
    .unwrap_err();
    assert!(error.is_session_persistence());
    assert_eq!(handle.model().0, "anthropic");
    // Making plan checkpoints writable again cannot adjudicate the failed
    // native model commit or clear its independent provider admission fence.
    handle.session_store().try_lock().unwrap().path = Some(path);
    for command in ["off", "save", "status"] {
        let output = run(controller.execute(&mut handle, command)).unwrap();
        assert!(output.contains("reopen the saved session"));
        assert!(output.contains("/plan save cannot clear this block"));
        assert!(!output.contains("Send a prompt to begin execution"));
        assert!(handle.session().ensure_provider_reentry_allowed().is_err());
    }
    assert_eq!(events.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&blocker).unwrap(), b"sentinel");
}

#[test]
fn saving_a_failed_entry_checkpoint_completes_the_planning_model_selection() {
    for explicit_selection in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut stored = Session::in_memory();
        stored.path = Some(path.clone());
        let (mut handle, events) = role_fixture(stored, true);
        let blocker = temp.path().join("not-a-directory");
        std::fs::write(&blocker, b"sentinel").unwrap();
        handle.session_store().try_lock().unwrap().path = Some(blocker.join("session.jsonl"));
        let mut controller = role_controller();
        run(controller.execute(&mut handle, "enter")).unwrap();
        assert!(controller.pending_model_selection.is_some());
        assert_eq!(handle.model().0, "openai");
        // Repeating an unchanged enter is not evidence that the checkpoint
        // was saved. Only the successful save below may finish selection.
        run(controller.execute(&mut handle, "enter")).unwrap();
        assert_eq!(handle.model().0, "openai");
        handle.session_store().try_lock().unwrap().path = Some(path);
        if explicit_selection {
            let (send, _receive) = std::sync::mpsc::channel();
            assert!(run(super::super::run_set_thinking_command(
                &mut handle,
                Some(ThinkingLevel::High),
                &send,
            )));
            controller.forget_model_restore();
        }
        let output = run(controller.execute(&mut handle, "save")).unwrap();
        assert!(controller.pending_model_selection.is_none());
        assert_eq!(
            handle.model().0,
            if explicit_selection {
                "openai"
            } else {
                "anthropic"
            }
        );
        assert_eq!(output.contains("Planning model:"), !explicit_selection);
        run(controller.execute(&mut handle, "off")).unwrap();
        assert_eq!(handle.model().0, "openai");
        assert_eq!(handle.thinking_level(), Some(ThinkingLevel::High));
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn a_replaced_session_never_receives_a_deferred_planning_selection() {
    let temp = tempfile::tempdir().unwrap();
    let mut stored = Session::in_memory();
    stored.path = Some(temp.path().join("session.jsonl"));
    let (mut first, _) = role_fixture(stored, true);
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, b"sentinel").unwrap();
    first.session_store().try_lock().unwrap().path = Some(blocker.join("session.jsonl"));
    let mut controller = role_controller();
    run(controller.execute(&mut first, "enter")).unwrap();
    assert!(controller.pending_model_selection.is_some());
    let (mut second, _) = role_fixture(Session::in_memory(), false);
    run(second.enter_plan_mode(&AgentCx::for_current_or_request())).unwrap();
    run(controller.execute(&mut second, "save")).unwrap();
    assert_eq!(second.model().0, "openai");
    assert!(controller.pending_model_selection.is_none());
}

#[test]
fn a_disabled_plan_provider_keeps_the_current_model_and_read_only_gate() {
    let (mut handle, events) = role_fixture(Session::in_memory(), false);
    let original = ModelSelection::current(&handle);
    let mut controller =
        role_controller().with_provider_policy(vec![" ANTHROPIC ".to_string()], Vec::new());
    let output = run(controller.execute(&mut handle, "enter")).unwrap();
    assert!(output.contains("Plan destination provider is disabled: anthropic"));
    assert_eq!(ModelSelection::current(&handle), original);
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::Planning
    );
    assert!(controller.model_restore.is_none());
    assert!(
        !handle
            .session()
            .agent
            .plan_state()
            .allows_effects(ToolEffects::write())
    );
    assert_eq!(events.load(Ordering::SeqCst), 0);
}

#[test]
fn plan_policy_uses_the_current_sessions_most_specific_workspace_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let allowed = root.join("allowed/child");
    let blocked = root.join("blocked");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&blocked).unwrap();
    let scopes = vec![
        crate::config::ModelScopeOverride {
            path: root.display().to_string(),
            disabled_providers: Some(vec!["anthropic".to_string()]),
            ..Default::default()
        },
        crate::config::ModelScopeOverride {
            path: root.join("allowed").display().to_string(),
            // Model scopes constrain the picker, not an explicitly assigned
            // role. Only the disabled-provider policy blocks this transition.
            enabled_models: Some(vec!["openai/ftui-plan-control-fixture".to_string()]),
            disabled_providers: Some(Vec::new()),
        },
    ];
    let mut controller = role_controller().with_provider_policy(Vec::new(), scopes.clone());
    for (cwd, permitted) in [(&allowed, true), (&blocked, false), (&allowed, true)] {
        let mut stored = Session::in_memory();
        stored.header.cwd = cwd.display().to_string();
        let (mut handle, events) = role_fixture(stored, false);
        let output = run(controller.execute(&mut handle, "enter")).unwrap();
        assert_eq!(output.contains("Planning model:"), permitted, "{output}");
        assert_eq!(
            handle.model().0,
            if permitted { "anthropic" } else { "openai" }
        );
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
    // A workspace override cannot lift the global disabled-provider policy.
    let mut stored = Session::in_memory();
    stored.header.cwd = allowed.display().to_string();
    let (mut handle, _) = role_fixture(stored, false);
    let mut controller =
        role_controller().with_provider_policy(vec!["anthropic".to_string()], scopes);
    let output = run(controller.execute(&mut handle, "enter")).unwrap();
    assert!(output.contains("Plan destination provider is disabled"));
    assert_eq!(handle.model().0, "openai");
}

#[test]
fn plan_policy_follows_a_workdir_attachment_instead_of_the_original_header() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let allowed = root.join("allowed");
    let blocked = root.join("blocked");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&blocked).unwrap();
    let scopes = vec![crate::config::ModelScopeOverride {
        path: blocked.display().to_string(),
        disabled_providers: Some(vec!["anthropic".to_string()]),
        ..Default::default()
    }];
    for (original, attached, permitted) in [(&allowed, &blocked, false), (&blocked, &allowed, true)]
    {
        let mut stored = Session::in_memory();
        stored.header.cwd = original.display().to_string();
        crate::session_workdir::attach_session_workdir(&mut stored, attached).unwrap();
        let (mut handle, events) = role_fixture(stored, false);
        let mut controller = role_controller().with_provider_policy(Vec::new(), scopes.clone());
        let output = run(controller.execute(&mut handle, "enter")).unwrap();
        assert_eq!(output.contains("Planning model:"), permitted, "{output}");
        assert_eq!(
            handle.model().0,
            if permitted { "anthropic" } else { "openai" }
        );
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}
