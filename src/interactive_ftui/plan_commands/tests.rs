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
        false,
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
    let output = render_change(&PlanChange {
        mode: PlanMode::Approved,
        changed: true,
        persistence: PlanPersistence::Unconfirmed {
            reason: "disk full\r\x1b".to_string(),
        },
    });
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
