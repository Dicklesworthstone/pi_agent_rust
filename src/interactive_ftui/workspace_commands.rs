//! OMP workspace commands on the default stack: `/rules`, `/omfg`,
//! `/commit`, `/review`, `/handoff`, `/approval`, `/advisor`, `/memory`,
//! `/hub` (alias `/agents`), `/security`, `/plugins`, and `/goal`. The work is
//! shared with the classic stack (`interactive::workspace_reports`); this
//! module routes them through the driver and renders the result.

mod goal_progress;

use std::path::Path;
use std::sync::mpsc::Sender;

use crate::interactive::PiMsg;
use crate::interactive::workspace_reports::{self, Report};
use crate::sdk::AgentSessionHandle;

/// Which workspace command the user ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceCommand {
    Rules,
    Omfg,
    Commit,
    Review,
    Handoff,
    Approval,
    Advisor,
    Memory,
    Hub,
    Security,
    Plugins,
    Goal,
}

impl WorkspaceCommand {
    /// Parse the command token (with its slash). `None` for anything else.
    pub fn parse(token: &str) -> Option<Self> {
        Some(match token.to_ascii_lowercase().as_str() {
            "/rules" => Self::Rules,
            "/omfg" => Self::Omfg,
            "/commit" => Self::Commit,
            "/review" => Self::Review,
            "/handoff" => Self::Handoff,
            "/approval" => Self::Approval,
            "/advisor" => Self::Advisor,
            "/memory" => Self::Memory,
            "/hub" | "/agents" => Self::Hub,
            "/security" => Self::Security,
            "/plugins" | "/packages" => Self::Plugins,
            "/goal" => Self::Goal,
            _ => return None,
        })
    }
}

/// The card when there is one (it carries the detail), else the status.
fn render(report: Report) -> PiMsg {
    PiMsg::System(report.card.unwrap_or(report.status))
}

/// Run `command` with `args` against the live session; `advisor` is the
/// label of the advisor this session was built with, if any, and
/// `packages` the launch package manager.
pub async fn run(
    command: WorkspaceCommand,
    args: &str,
    handle: &mut AgentSessionHandle,
    cwd: &Path,
    advisor: Option<&str>,
    packages: Option<&crate::package_manager::PackageManager>,
    agent_tx: &Sender<PiMsg>,
) {
    let msg = match command {
        WorkspaceCommand::Rules => render(workspace_reports::rules(cwd, args)),
        WorkspaceCommand::Omfg => render(workspace_reports::omfg(cwd, args)),
        WorkspaceCommand::Commit => render(workspace_reports::commit(cwd, args)),
        WorkspaceCommand::Review => render(workspace_reports::review(cwd, args)),
        WorkspaceCommand::Advisor => render(workspace_reports::advisor(advisor, args)),
        WorkspaceCommand::Memory => render(workspace_reports::memory(
            cwd,
            args,
            handle.session().agent.secrets_settings(),
        )),
        WorkspaceCommand::Hub => render(workspace_reports::hub(args)),
        WorkspaceCommand::Security => render(workspace_reports::security(cwd, args)),
        WorkspaceCommand::Plugins => packages.map_or_else(
            || PiMsg::System(String::from("The package manager is not available here.")),
            |manager| render(workspace_reports::plugins(manager)),
        ),
        WorkspaceCommand::Handoff => {
            match handle
                .with_session(|session| workspace_reports::handoff(session, args))
                .await
            {
                Ok(report) => render(report),
                Err(err) => PiMsg::AgentError(format!("handoff: {err}")),
            }
        }
        WorkspaceCommand::Approval => run_approval(handle, args).await,
        WorkspaceCommand::Goal => {
            // Observe only this command's existing SDK events. The guard
            // unsubscribes on success, error or future drop; it has no goal
            // execution, input, approval or continuation authority.
            let progress = goal_progress::Observation::attach(handle.listeners(), agent_tx);
            let result = Box::pin(crate::goal::terminal::run(handle, args)).await;
            progress.finish();
            match result {
                Ok(text) => PiMsg::System(text),
                Err(error) => PiMsg::AgentError(format!("goal: {error}")),
            }
        }
    };
    let _ = agent_tx.send(msg);
}

async fn run_approval(handle: &mut AgentSessionHandle, args: &str) -> PiMsg {
    let Some(state) = handle.session().agent.approval_state() else {
        return PiMsg::System(String::from("Tool approval state not configured"));
    };
    let (report, changed) = workspace_reports::approval(&state, args);
    if let Some(mode) = changed
        && let Err(err) = handle.record_approval_mode(mode).await
    {
        return PiMsg::AgentError(format!(
            "{} (not recorded in the session: {err})",
            report.status
        ));
    }
    render(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_admits_exactly_the_workspace_commands() {
        for (token, expected) in [
            ("/rules", WorkspaceCommand::Rules),
            ("/omfg", WorkspaceCommand::Omfg),
            ("/commit", WorkspaceCommand::Commit),
            ("/review", WorkspaceCommand::Review),
            ("/handoff", WorkspaceCommand::Handoff),
            ("/approval", WorkspaceCommand::Approval),
            ("/ADVISOR", WorkspaceCommand::Advisor),
            ("/memory", WorkspaceCommand::Memory),
            ("/hub", WorkspaceCommand::Hub),
            ("/agents", WorkspaceCommand::Hub),
            ("/security", WorkspaceCommand::Security),
            ("/plugins", WorkspaceCommand::Plugins),
            ("/packages", WorkspaceCommand::Plugins),
        ] {
            assert_eq!(WorkspaceCommand::parse(token), Some(expected), "{token}");
        }
        assert_eq!(WorkspaceCommand::parse("/rule"), None);
        assert_eq!(WorkspaceCommand::parse("rules"), None);
    }

    #[test]
    fn default_terminal_routes_goal_without_claiming_extension_prefixes() {
        use super::super::{PiFtuiModel, UiCommand};
        let (_agent_tx, agent_rx) = std::sync::mpsc::channel();
        let (submit_tx, submit_rx) = std::sync::mpsc::channel();
        let mut model = PiFtuiModel::new(agent_rx).with_submit_channel(submit_tx);
        assert!(model.route_slash_command("/GoAl status"));
        assert_eq!(submit_rx.try_recv().unwrap(), UiCommand::Workspace {
            command: WorkspaceCommand::Goal,
            args: "status".to_string(),
        });
        model.busy = None; // The first command's driver reply ends its busy state.
        assert!(model.route_slash_command("/goalkeeper exact Argument"));
        assert_eq!(submit_rx.try_recv().unwrap(), UiCommand::ExtensionCommand {
            name: "goalkeeper".to_string(),
            args: "exact Argument".to_string(),
        });
        assert_eq!(WorkspaceCommand::parse("goal"), None);
    }

    #[test]
    fn default_driver_goal_controls_change_the_actual_session_without_agent_work() {
        use crate::agent::{Agent, AgentConfig, AgentSession};
        use crate::compaction::ResolvedCompactionSettings;
        use crate::providers::openai::OpenAIProvider;
        use crate::sdk::EventListeners;
        use crate::session::Session;
        use crate::tools::ToolRegistry;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let provider = Arc::new(OpenAIProvider::new("terminal-goal-routing")
            .with_base_url("http://127.0.0.1:1/v1"));
        let agent = Agent::new(provider, ToolRegistry::new(&[], Path::new("."), None), AgentConfig::default());
        let session = AgentSession::new(agent, Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
            false, ResolvedCompactionSettings::default());
        let events = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&events);
        let listeners = EventListeners::default();
        let _ = listeners.subscribe(Arc::new(move |_| { counted.fetch_add(1, Ordering::SeqCst); }));
        let mut handle = AgentSessionHandle::from_session_with_listeners(session, listeners);
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.block_on(run(WorkspaceCommand::Goal, "status", &mut handle, Path::new("."), None, None, &sender));
        let PiMsg::System(status) = receiver.try_recv().unwrap() else { panic!("goal status"); };
        let token = status.lines().find_map(|line| line.strip_prefix("Review: ")).unwrap();
        runtime.block_on(run(WorkspaceCommand::Goal, &format!("create {token} -- routed objective"),
            &mut handle, Path::new("."), None, None, &sender));
        let PiMsg::System(created) = receiver.try_recv().unwrap() else { panic!("goal creation"); };
        assert!(created.contains("Memory-only"));
        assert_eq!(handle.current_goal().unwrap().unwrap().objective(), "routed objective");
        runtime.block_on(run(WorkspaceCommand::Goal, &format!("drop {token} -- stale queued command"),
            &mut handle, Path::new("."), None, None, &sender));
        let PiMsg::AgentError(error) = receiver.try_recv().unwrap() else { panic!("stale receipt refused"); };
        assert!(error.contains("GOAL_STALE"));
        assert_eq!(handle.current_goal().unwrap().unwrap().status(), crate::goal::GoalStatus::Active);
        assert_eq!(events.load(Ordering::SeqCst), 0);
    }
}
