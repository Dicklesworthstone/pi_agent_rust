//! OMP workspace commands on the default stack: `/rules`, `/omfg`,
//! `/commit`, `/review`, `/handoff`, `/approval`, `/advisor`, `/memory`,
//! `/hub` (alias `/agents`), `/security`, `/plugins`. The work is
//! shared with the classic stack (`interactive::workspace_reports`); this
//! module routes them through the driver and renders the result.

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
        WorkspaceCommand::Memory => render(workspace_reports::memory(cwd, args)),
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
}
