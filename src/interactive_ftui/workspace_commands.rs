//! OMP workspace commands on the default stack: `/rules`, `/omfg`,
//! `/commit`, `/review`, `/handoff`, `/approval`, `/advisor`, `/memory`,
//! `/hub` (alias `/agents`), `/security`, `/plugins`, and `/goal`. The work is
//! shared with the classic stack (`interactive::workspace_reports`); this
//! module routes them through the driver and renders the result.

mod goal_progress;
mod model_handoff;

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
        WorkspaceCommand::Review => {
            run_review(args, handle, cwd, None, agent_tx).await;
            return;
        }
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
        WorkspaceCommand::Handoff if model_handoff::is_option_command(args) => {
            match Box::pin(model_handoff::run(handle, args)).await {
                Ok(preview) => PiMsg::System(preview),
                Err(error) => PiMsg::AgentError(format!("handoff: {error}")),
            }
        }
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
            // Bind only this invocation's control before native dispatch.
            // Observed receiver loss or abandoned observation seals further
            // continuation; native abort-and-drain still owns cleanup.
            let progress = goal_progress::Observation::attach(handle.listeners(), agent_tx);
            let result = Box::pin(crate::goal::terminal::run_with_control(
                handle, args, progress.control_publisher(),
            )).await;
            progress.finish();
            match result {
                Ok(text) => PiMsg::System(text),
                Err(error) => PiMsg::AgentError(format!("goal: {error}")),
            }
        }
    };
    let _ = agent_tx.send(msg);
}

pub(super) async fn run_review(
    args: &str,
    handle: &AgentSessionHandle,
    cwd: &Path,
    abort: Option<&crate::agent::AbortSignal>,
    agent_tx: &Sender<PiMsg>,
) {
    let result = workspace_reports::prepare_review(cwd, args, &handle.session().agent);
    let msg = match result {
        Ok(request) => render(request.run(abort).await),
        Err(error) => PiMsg::AgentError(format!("Review failed: {error}")),
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

    struct ReviewRouteProvider {
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl crate::provider::Provider for ReviewRouteProvider {
        fn name(&self) -> &str { "review-route" }
        fn api(&self) -> &str { "review-route" }
        fn model_id(&self) -> &str { "mock-model" }

        async fn stream(
            &self,
            context: &crate::provider::Context<'_>,
            _: &crate::provider::StreamOptions,
        ) -> crate::error::Result<std::pin::Pin<Box<dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>> + Send>>> {
            use crate::model::{AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent};
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(context.tools.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- The actual terminal dispatcher must supply a tool-free reviewer context.
            assert_eq!(context.messages.len(), 1); // ubs:ignore[rust.panic.assert-macros] -- The dispatcher must isolate review input from the live conversation.
            if self.fail {
                return Err(crate::error::Error::validation("mock reviewer unavailable"));
            }
            let event = StreamEvent::Done {
                reason: StopReason::Stop,
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(r#"{"findings":[{"severity":"P1","confidence":0.95,"lineStart":1,"lineEnd":1,"category":"correctness","title":"Reject the boundary index","rationale":"The new condition accepts index equal to count and then indexes past the end."}]}"#))],
                    ..Default::default()
                },
            };
            Ok(Box::pin(futures::stream::iter([Ok(event)])))
        }
    }

    fn review_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("review repository"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Dispatcher tests require an isolated native Git repository.
        for args in [
            vec!["init"],
            vec!["config", "user.name", "Review Test"],
            vec!["config", "user.email", "review@example.invalid"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(std::process::Command::new("git").args(args).current_dir(temp.path()).output().expect("git setup").status.success()); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Repository initialization must succeed before exercising the dispatcher.
        }
        std::fs::write(temp.path().join("bounds.rs"), "if index < count { use_item(items[index]); }\n").expect("base source"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Seed the valid original source for the real Git diff fixture.
        for args in [vec!["add", "bounds.rs"], vec!["commit", "-m", "base"]] {
            assert!(std::process::Command::new("git").args(args).current_dir(temp.path()).output().expect("git commit").status.success()); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- A committed baseline is required to distinguish introduced code.
        }
        std::fs::write(temp.path().join("bounds.rs"), "if index <= count { use_item(items[index]); }\n").expect("changed source"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Introduce the exact bounds defect used by the model fixture.
        temp
    }

    fn review_handle(provider: std::sync::Arc<ReviewRouteProvider>, cwd: &Path) -> AgentSessionHandle {
        let agent = crate::agent::Agent::new(provider, crate::tools::ToolRegistry::new(&[], cwd, None), crate::agent::AgentConfig::default());
        let session = crate::agent::AgentSession::new(
            agent,
            std::sync::Arc::new(asupersync::sync::Mutex::new(crate::session::Session::in_memory())),
            false,
            crate::compaction::ResolvedCompactionSettings::default(),
        );
        AgentSessionHandle::from_session_with_listeners(session, crate::sdk::EventListeners::default())
    }

    #[test]
    fn review_dispatcher_uses_session_model_and_keeps_offline_explicit() {
        let fixture = review_fixture();
        let provider = std::sync::Arc::new(ReviewRouteProvider {
            calls: std::sync::atomic::AtomicUsize::new(0), fail: false,
        });
        let mut handle = review_handle(std::sync::Arc::clone(&provider), fixture.path());
        let original_messages = handle.session().agent.messages().len();
        let (sender, receiver) = std::sync::mpsc::channel();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The actual async terminal dispatcher requires its runtime.
        runtime.block_on(run(WorkspaceCommand::Review, "", &mut handle, fixture.path(), None, None, &sender));
        let PiMsg::System(report) = receiver.try_recv().expect("review result") else { panic!("review card"); }; // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.ownership.panic-macro] -- Successful dispatch must publish a system report card.
        assert!(report.contains("BLOCK")); // ubs:ignore[rust.panic.assert-macros] -- The model's confirmed defect must block the displayed verdict.
        assert!(report.contains("Reject the boundary index")); // ubs:ignore[rust.panic.assert-macros] -- The report must contain the actual provider finding.
        assert!(report.contains("bounds.rs:1")); // ubs:ignore[rust.panic.assert-macros] -- The terminal card must retain the validated actionable location.
        assert!(report.contains("model:review-route/mock-model")); // ubs:ignore[rust.panic.assert-macros] -- The selected session model must be identified in the report.
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- One text file must result in one real provider call.
        assert_eq!(handle.session().agent.messages().len(), original_messages); // ubs:ignore[rust.panic.assert-macros] -- The review conversation must not mutate the parent's model context.

        runtime.block_on(run(WorkspaceCommand::Review, "--offline", &mut handle, fixture.path(), None, None, &sender));
        let PiMsg::System(report) = receiver.try_recv().expect("offline result") else { panic!("offline card"); }; // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.ownership.panic-macro] -- Explicit offline dispatch must still return its labeled report card.
        assert!(report.contains("offline-heuristics")); // ubs:ignore[rust.panic.assert-macros] -- Offline results must be distinguishable from model reviews.
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1, "offline must not call the model"); // ubs:ignore[rust.panic.assert-macros] -- The offline invocation must not add any network request.
        eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"default_dispatcher","outcome":"current model called; transcript unchanged; offline explicit"}));
    }

    #[test]
    fn review_dispatcher_reports_provider_failure_without_a_ship_verdict() {
        let fixture = review_fixture();
        let provider = std::sync::Arc::new(ReviewRouteProvider {
            calls: std::sync::atomic::AtomicUsize::new(0), fail: true,
        });
        let mut handle = review_handle(std::sync::Arc::clone(&provider), fixture.path());
        let (sender, receiver) = std::sync::mpsc::channel();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Exercise provider failure through the actual async dispatcher.
        runtime.block_on(run(WorkspaceCommand::Review, "", &mut handle, fixture.path(), None, None, &sender));
        let PiMsg::System(error) = receiver.try_recv().expect("review failure") else { panic!("review failure status"); }; // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.ownership.panic-macro] -- Failed provider execution must produce a visible failure status.
        assert!(error.contains("Review failed:")); // ubs:ignore[rust.panic.assert-macros] -- The terminal must identify review execution failure.
        assert!(error.contains("mock reviewer unavailable")); // ubs:ignore[rust.panic.assert-macros] -- The provider failure must remain observable.
        assert!(!error.contains("SHIP")); // ubs:ignore[rust.panic.assert-macros] -- Failure must never be converted into a successful verdict.
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Failure must not silently invoke a fallback provider.
        eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"dispatcher_failure","outcome":"provider failure displayed; no successful verdict"}));
    }

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
