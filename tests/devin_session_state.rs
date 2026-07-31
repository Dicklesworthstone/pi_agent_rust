//! Session persistence coverage for Devin modes and scopes.

use pi::devin::{AgentMode, DevinSessionState, PermissionMode, SandboxStatus, ScopeAccess};
use pi::session::Session;

#[test]
fn devin_state_round_trips_through_custom_session_entries() {
    let workspace = tempfile::tempdir().expect("workspace");
    let external = tempfile::tempdir().expect("external scope");
    let mut state = DevinSessionState::new("session-1", workspace.path());
    state.agent_mode = AgentMode::Plan;
    state.permission_mode = PermissionMode::Smart;
    state.sandbox_status = SandboxStatus::Available;
    state.grant_scope(external.path(), ScopeAccess::Read);

    let mut session = Session::in_memory();
    session
        .append_devin_state(&state)
        .expect("append Devin session state");
    let restored = session
        .latest_devin_state()
        .expect("parse Devin session state")
        .expect("Devin session state exists");

    assert_eq!(restored.session_id, "session-1");
    assert_eq!(restored.workspace, workspace.path());
    assert_eq!(restored.agent_mode, AgentMode::Plan);
    assert_eq!(restored.permission_mode, PermissionMode::Smart);
    assert_eq!(restored.sandbox_status, SandboxStatus::Available);
    assert_eq!(restored.scopes.len(), 1);
    assert_eq!(restored.scopes[0].access, ScopeAccess::Read);
}

#[test]
fn latest_devin_state_wins_on_current_branch() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut session = Session::in_memory();
    let mut state = DevinSessionState::new("session-1", workspace.path());
    session
        .append_devin_state(&state)
        .expect("append initial Devin state");

    state.agent_mode = AgentMode::Ask;
    session
        .append_devin_state(&state)
        .expect("append updated Devin state");

    assert_eq!(
        session
            .latest_devin_state()
            .expect("parse Devin session state")
            .expect("Devin session state exists")
            .agent_mode,
        AgentMode::Ask
    );
}
