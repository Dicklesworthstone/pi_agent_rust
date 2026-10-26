//! Native Bash, the Agent permission gate and real ACP response routing.
use super::*;
use crate::agent::{Agent, AgentConfig};
use crate::model::{AssistantMessage, StopReason, StreamEvent};
use crate::tools::{BashTool, JobsTool};
use std::cell::RefCell;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, sync_channel};

struct Fixture {
    root: tempfile::TempDir,
    tools: ToolRegistry,
    output: AcpOutput,
    receiver: Receiver<String>,
    permissions: AcpPermissionClient,
    prompt: Option<AcpPromptGuard>,
    abort: AbortSignal,
    frames: RefCell<Vec<Value>>,
}

impl Fixture {
    fn new(enabled: bool) -> Self {
        let root = tempfile::tempdir().expect("editor workspace");
        let (sender, receiver) = sync_channel(128);
        let output = AcpOutput::new(sender);
        output.shared.terminals.negotiate(&json!({"clientCapabilities":{"terminal":enabled}}));
        let permissions = AcpPermissionClient {
            out_tx: output.clone(),
            pending: Arc::clone(&output.shared.pending_permissions),
            request_counter: Arc::new(AtomicU64::new(0)),
            timeout: Duration::from_secs(5),
            cx: AgentCx::for_current_or_request(),
        };
        let tools = ToolRegistry::from_tools(vec![
            Box::new(BashTool::with_shell(root.path(), Some("bash".into()), Some("export ACP_BRIDGE_TEST=1".into()))),
            Box::new(JobsTool::new()),
        ]);
        bind(&tools, Some(&permissions), "editor-session", root.path()).unwrap();
        let (abort, signal) = AbortHandle::new();
        let prompt = output.begin_prompt("editor-session".into(), abort).unwrap();
        Self {
            root, tools, output, receiver, permissions, prompt: Some(prompt), abort: signal,
            frames: RefCell::new(Vec::new()),
        }
    }

    fn client(&self) -> Client {
        Client {
            output: self.output.clone(), session: "editor-session".into(),
            cwd: crate::extensions::safe_canonicalize(self.root.path()),
            rpc_timeout: Duration::from_millis(250),
            output_timeout: Duration::from_millis(250),
            cleanup_timeout: Duration::from_millis(250),
            poll_interval: Duration::from_millis(5),
        }
    }

    fn command(&self, timeout: Option<Duration>) -> TerminalCommand {
        TerminalCommand {
            command: "bash".into(), args: vec!["-c".into(), "printf editor".into()],
            env: Vec::new(), cwd: self.root.path().to_path_buf(), timeout,
        }
    }

    fn inspect(&self, raw: &str, expected: &str) -> Option<Value> {
        let frame: Value = serde_json::from_str(raw).expect("JSON-RPC frame");
        self.frames.borrow_mut().push(frame.clone());
        if frame["method"] == expected {
            assert_eq!(frame["params"]["sessionId"], "editor-session");
            Some(frame)
        } else {
            assert!(matches!(frame["method"].as_str(), Some("$/cancel_request" | "session/update")),
                "expected {expected}, received {frame}");
            None
        }
    }

    fn frame(&self, method: &str) -> Value {
        loop {
            if let Some(frame) = self.inspect(&self.receiver.try_recv().expect("outbound request"), method) {
                return frame;
            }
        }
    }

    async fn await_frame<F: Future + ?Sized>(&self, mut pending: Pin<&mut F>, method: &str) -> Value {
        timeout(wall_now(), Duration::from_secs(5), Box::pin(futures::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending(), "operation completed before {method}");
            loop {
                match self.receiver.try_recv() {
                    Ok(raw) => {
                        if let Some(frame) = self.inspect(&raw, method) {
                            return std::task::Poll::Ready(frame);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.waker().wake_by_ref();
                        return std::task::Poll::Pending;
                    }
                    Err(error) => panic!("editor output closed: {error}"),
                }
            }
        }))).await.expect("editor request deadline")
    }

    fn reply(&self, frame: &Value, result: Value) {
        assert!(self.route(json!({"jsonrpc":"2.0", "id":frame["id"], "result":result})));
    }

    fn route(&self, response: Value) -> bool {
        self.output.shared.terminals.route_response(
            &self.output, &response, &AgentCx::for_current_or_request(),
        )
    }

    async fn allocate<F: Future + ?Sized>(&self, mut pending: Pin<&mut F>) -> (Value, Value) {
        let create = self.await_frame(pending.as_mut(), "terminal/create").await;
        self.reply(&create, json!({"terminalId":"editor-terminal"}));
        let wait = self.await_frame(pending.as_mut(), "terminal/wait_for_exit").await;
        let output = self.await_frame(pending.as_mut(), "terminal/output").await;
        (wait, output)
    }

    async fn finish<F: Future + ?Sized>(
        &self, mut pending: Pin<&mut F>, wait: &Value, exit: Value, text: &str,
    ) -> Value {
        self.reply(wait, exit.clone());
        let output = self.await_frame(pending.as_mut(), "terminal/output").await;
        self.reply(&output, json!({"output":text, "truncated":false, "exitStatus":exit}));
        self.await_frame(pending.as_mut(), "terminal/release").await
    }
}

async fn poll_pending<F: Future + ?Sized>(mut pending: Pin<&mut F>) {
    futures::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    }).await;
}

fn text(output: &ToolOutput) -> &str {
    match output.content.as_slice() {
        [ContentBlock::Text(text)] => &text.text,
        _ => panic!("expected one text output"),
    }
}

#[test]
fn native_foreground_uses_shell_argv_streams_output_and_releases_the_editor_terminal() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let tools = fixture.tools.clone_shallow();
        let updates = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&updates);
        let command = "printf '%s\\n' \"a b\" | cat; printf forbidden > local-marker";
        let mut pending = Box::pin(tools.get("bash").unwrap().execute(
            "native-bash", json!({"command":command}),
            Some(Box::new(move |update| observed.lock().unwrap().push(update))),
        ));
        let create = fixture.await_frame(pending.as_mut(), "terminal/create").await;
        assert_eq!(create["params"]["command"], "bash");
        assert_eq!(create["params"]["args"][0], "-c");
        assert_eq!(create["params"]["args"].as_array().unwrap().len(), 2);
        let shell_line = create["params"]["args"][1].as_str().unwrap();
        assert!(shell_line.starts_with("trap 'code=$?; wait; exit $code' EXIT\n"));
        assert!(shell_line.contains("export ACP_BRIDGE_TEST=1\n"));
        assert!(shell_line.ends_with(command));
        assert_eq!(create["params"]["cwd"], serde_json::to_value(crate::extensions::safe_canonicalize(fixture.root.path())).unwrap());
        assert_eq!(create["params"]["outputByteLimit"], DEFAULT_MAX_BYTES);
        assert!(create["params"]["env"].as_array().unwrap().iter().any(|env| env["name"] == "GIT_TERMINAL_PROMPT" && env["value"] == "0"));
        fixture.reply(&create, json!({"terminalId":"editor-terminal"}));
        let wait = fixture.await_frame(pending.as_mut(), "terminal/wait_for_exit").await;
        let output = fixture.await_frame(pending.as_mut(), "terminal/output").await;
        fixture.reply(&output, json!({"output":"first line\n", "truncated":false}));
        poll_pending(pending.as_mut()).await;
        assert!(updates.lock().unwrap().iter().any(|update| {
            update.details.as_ref().is_some_and(|details| details["terminalId"] == "editor-terminal")
                && update.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text == "first line\n"))
        }));
        let release = fixture.finish(pending.as_mut(), &wait, json!({"exitCode":0}), "first line\nlast line\n").await;
        fixture.reply(&release, json!({}));
        let result = pending.await.unwrap();
        assert!(!result.is_error);
        assert!(text(&result).starts_with("first line\nlast line\n"));
        let details = result.details.unwrap();
        assert_eq!(details["execution"], "editor_terminal");
        assert_eq!(details["cleanup"], "released");
        assert_eq!(details["shellState"]["mode"], "isolated");
        assert_eq!(details["timeoutMs"], 120_000);
        assert!(!fixture.root.path().join("local-marker").exists());
        assert!(!fixture.output.shared.terminals.has_pending());
        assert!(!fixture.frames.borrow().iter().any(|frame| frame["method"] == "terminal/kill"));
    });
}

#[test]
fn native_mediation_blocks_before_editor_allocation() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let tools = ToolRegistry::from_tools(vec![Box::new(BashTool::new(fixture.root.path()).with_mediation(Some(crate::config::BashSettings {
            mediation: Some("block-high".into()), mediation_dcg: Some(false), ..Default::default()
        })))]);
        bind(&tools, Some(&fixture.permissions), "editor-session", fixture.root.path()).unwrap();
        std::fs::create_dir(fixture.root.path().join("doomed")).unwrap();
        std::fs::write(fixture.root.path().join("doomed/keep.txt"), "unchanged").unwrap();
        let result = tools.get("bash").unwrap().execute("blocked", json!({"command":"rm -rf doomed"}), None).await.unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("MEDIATION BLOCK"));
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("doomed/keep.txt")).unwrap(), "unchanged");
        assert!(fixture.receiver.try_recv().is_err());
        assert!(!fixture.output.shared.terminals.has_pending());
    });
}

#[test]
fn client_errors_are_sanitized_and_never_retry_the_command_locally() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let mut pending = Box::pin(fixture.tools.get("bash").unwrap().execute(
            "failed-create", json!({"command":"printf forbidden > local-marker"}), None,
        ));
        let create = fixture.await_frame(pending.as_mut(), "terminal/create").await;
        assert!(fixture.route(json!({"jsonrpc":"2.0", "id":create["id"], "error":{"code":-32000,"message":"private-client-token"}})));
        let result = pending.await.unwrap();
        assert!(result.is_error);
        assert!(!text(&result).contains("private-client-token"));
        assert!(!fixture.root.path().join("local-marker").exists());
        assert!(!fixture.output.shared.terminals.has_pending());
        assert!(fixture.receiver.try_recv().is_err());
    });
}

#[test]
fn dropped_allocation_cleans_late_reply_and_fences_only_its_session_until_valid_release() {
    asupersync::test_utils::run_test(|| async {
        let mut fixture = Fixture::new(true);
        let create = {
            let mut pending = Box::pin(fixture.tools.get("bash").unwrap().execute(
                "late-create", json!({"command":"printf forbidden > local-marker"}), None,
            ));
            let create = fixture.await_frame(pending.as_mut(), "terminal/create").await;
            drop(pending);
            create
        };
        drop(fixture.prompt.take());
        assert!(fixture.output.shared.terminals.has_unsettled("editor-session"));
        let (abort, _) = AbortHandle::new();
        assert!(fixture.output.begin_prompt("editor-session".into(), abort).is_err());
        let (abort, _) = AbortHandle::new();
        let other = fixture.output.begin_prompt("other-session".into(), abort).unwrap();
        assert!(!fixture.output.shared.terminals.has_unsettled("other-session"));
        fixture.reply(&create, json!({"terminalId":""}));
        assert!(fixture.output.shared.terminals.has_unsettled("editor-session"));
        assert!(fixture.receiver.try_recv().is_err());
        fixture.reply(&create, json!({"terminalId":"late-terminal"}));
        let kill = fixture.frame("terminal/kill");
        let release = fixture.frame("terminal/release");
        assert_eq!(kill["params"]["terminalId"], "late-terminal");
        assert_eq!(release["params"]["terminalId"], "late-terminal");
        fixture.reply(&kill, json!({}));
        fixture.reply(&release, json!(null));
        assert!(fixture.output.shared.terminals.has_unsettled("editor-session"));
        fixture.reply(&release, json!({}));
        assert!(!fixture.output.shared.terminals.has_pending());
        assert!(!fixture.route(json!({"jsonrpc":"2.0", "id":create["id"], "result":{"terminalId":"duplicate"}})));
        let (abort, _) = AbortHandle::new();
        let resumed = fixture.output.begin_prompt("editor-session".into(), abort).unwrap();
        assert!(!fixture.root.path().join("local-marker").exists());
        drop(resumed);
        drop(other);
    });
}

#[test]
fn timeout_zero_remains_cancellable_and_cleanup_acknowledgment_retires_waiters() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let mut pending = Box::pin(fixture.tools.get("bash").unwrap().execute(
            "cancelled-terminal", json!({"command":"sleep 60", "timeout":0}), None,
        ));
        let (wait, output) = fixture.allocate(pending.as_mut()).await;
        fixture.output.abort_prompt("editor-session");
        let kill = fixture.await_frame(pending.as_mut(), "terminal/kill").await;
        let release = fixture.await_frame(pending.as_mut(), "terminal/release").await;
        fixture.reply(&kill, json!({}));
        fixture.reply(&release, json!({}));
        let result = pending.await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.details.as_ref().unwrap()["status"], "cancelled");
        assert_eq!(result.details.as_ref().unwrap()["cleanup"], "released");
        assert!(result.details.as_ref().unwrap()["timeoutMs"].is_null());
        assert!(!fixture.output.shared.terminals.has_pending());
        assert!(!fixture.route(json!({"jsonrpc":"2.0", "id":wait["id"], "result":{"exitCode":0}})));
        assert!(!fixture.route(json!({"jsonrpc":"2.0", "id":output["id"], "result":{"output":"late", "truncated":false}})));
    });
}

#[test]
fn command_deadline_kills_and_releases_without_waiting_for_the_client_to_exit() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let client = fixture.client();
        let mut pending = Box::pin(client.execute(fixture.command(Some(Duration::from_millis(20))), None));
        fixture.allocate(pending.as_mut()).await;
        let kill = fixture.await_frame(pending.as_mut(), "terminal/kill").await;
        let release = fixture.await_frame(pending.as_mut(), "terminal/release").await;
        fixture.reply(&kill, json!({}));
        fixture.reply(&release, json!({}));
        let result = pending.await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.details.as_ref().unwrap()["status"], "timedOut");
        assert_eq!(result.details.as_ref().unwrap()["cleanup"], "released");
        assert!(!fixture.output.shared.terminals.has_pending());
    });
}

#[test]
fn unacknowledged_release_is_bounded_aborts_the_prompt_and_remains_session_owned() {
    asupersync::test_utils::run_test(|| async {
        let mut fixture = Fixture::new(true);
        let release = {
            let mut client = fixture.client();
            client.cleanup_timeout = Duration::from_millis(20);
            let mut pending = Box::pin(client.execute(fixture.command(Some(Duration::from_secs(2))), None));
            let (wait, output) = fixture.allocate(pending.as_mut()).await;
            fixture.reply(&output, json!({"output":"done", "truncated":false}));
            poll_pending(pending.as_mut()).await;
            let release = fixture.finish(pending.as_mut(), &wait, json!({"exitCode":0}), "done").await;
            let result = pending.await.unwrap();
            assert!(result.is_error);
            assert_eq!(result.details.as_ref().unwrap()["cleanup"], "unconfirmed");
            assert!(text(&result).contains("may still be running"));
            release
        };
        assert!(fixture.abort.is_aborted());
        drop(fixture.prompt.take());
        let (abort, _) = AbortHandle::new();
        assert!(fixture.output.begin_prompt("editor-session".into(), abort).is_err());
        fixture.reply(&release, json!({}));
        assert!(!fixture.output.shared.terminals.has_pending());
        let (abort, _) = AbortHandle::new();
        let resumed = fixture.output.begin_prompt("editor-session".into(), abort).unwrap();
        drop(resumed);
    });
}

#[test]
fn oversized_client_output_fails_closed_and_is_never_returned_to_the_model() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let client = fixture.client();
        let mut pending = Box::pin(client.execute(fixture.command(Some(Duration::from_secs(2))), None));
        let (_, output) = fixture.allocate(pending.as_mut()).await;
        fixture.reply(&output, json!({"output":"s".repeat(DEFAULT_MAX_BYTES + 1), "truncated":false}));
        let kill = fixture.await_frame(pending.as_mut(), "terminal/kill").await;
        let release = fixture.await_frame(pending.as_mut(), "terminal/release").await;
        fixture.reply(&kill, json!({}));
        fixture.reply(&release, json!({}));
        let result = pending.await.unwrap();
        assert!(result.is_error);
        assert!(text(&result).len() < 1024);
        assert!(!fixture.output.shared.terminals.has_pending());
    });
}

#[test]
fn output_tail_limits_and_unsuccessful_exit_statuses_survive_terminal_mapping() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let client = fixture.client();
        let output = format!("old-first\n{}new-last\n", "line\n".repeat(DEFAULT_MAX_LINES));
        let mut pending = Box::pin(client.execute(fixture.command(Some(Duration::from_secs(2))), None));
        let (wait, first) = fixture.allocate(pending.as_mut()).await;
        fixture.reply(&first, json!({"output":"running", "truncated":false}));
        poll_pending(pending.as_mut()).await;
        let release = fixture.finish(pending.as_mut(), &wait, json!({"exitCode":17}), &output).await;
        fixture.reply(&release, json!({}));
        let result = pending.await.unwrap();
        assert!(result.is_error);
        assert!(!text(&result).contains("old-first"));
        assert!(text(&result).contains("new-last"));
        assert_eq!(result.details.as_ref().unwrap()["exitCode"], 17);
        assert_eq!(result.details.as_ref().unwrap()["truncated"], true);
        let emit = build_acp_event_handler(fixture.output.clone(), "editor-session".into());
        emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: "bash-card".into(), tool_name: "bash".into(), args: json!({}), partial_result: result.clone(),
        });
        let update = fixture.frame("session/update");
        assert!(update["params"]["update"]["content"].as_array().unwrap().iter().any(|content| content == &json!({"type":"terminal", "terminalId":"editor-terminal"})));
        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: "bash-card".into(), tool_name: "bash".into(), result, is_error: true,
        });
        let ended = fixture.frame("session/update");
        assert_eq!(ended["params"]["update"]["status"], "failed");
        assert!(ended["params"]["update"]["content"].as_array().unwrap().iter().any(|content| content["type"] == "terminal"));
        assert!(tool_content(&[], Some(&json!({"terminalId":"bad\nidentifier"}))).is_empty());
    });
}

#[test]
fn workspace_argument_environment_and_capacity_bounds_precede_publication() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        let client = fixture.client();
        let mut command = fixture.command(None);
        command.cwd = fixture.root.path().parent().unwrap().to_path_buf();
        assert!(client.execute(command, None).await.is_err());
        let mut command = fixture.command(None);
        command.args = vec!["x".repeat(MAX_COMMAND_BYTES + 1)];
        assert!(client.execute(command, None).await.is_err());
        let mut command = fixture.command(None);
        command.env = vec![("INVALID=NAME".into(), "value".into())];
        assert!(client.execute(command, None).await.is_err());
        let leases = (0..MAX_RUNS).map(|_| {
            fixture.output.shared.terminals.start(&fixture.output, "editor-session").unwrap()
        }).collect::<Vec<_>>();
        assert!(fixture.output.shared.terminals.start(&fixture.output, "editor-session").is_err());
        drop(leases);
        assert!(!fixture.output.shared.terminals.has_pending());
        assert!(fixture.receiver.try_recv().is_err());
    });
}

#[test]
fn editor_allocation_preserves_the_native_owners_spawn_authority_and_cancellation() {
    asupersync::test_utils::run_test(|| async {
        type WithoutSpawn = asupersync::cx::cap::CapSet<false, true, true, true, true>;
        let fixture = Fixture::new(true);
        let client = fixture.client();
        let owner = {
            let restricted = asupersync::Cx::for_request().restrict::<WithoutSpawn>();
            let _guard = restricted.set_current_restricted();
            AgentCx::for_current_or_request()
        };
        assert!(owner.capabilities().io);
        assert!(!owner.capabilities().spawn);
        let error = owner.with_current(client.execute(fixture.command(None), None)).await.unwrap_err();
        assert!(error.to_string().contains("spawn capabilities"));
        let cancelled = AgentCx::for_request();
        cancelled.cx().set_cancel_requested(true);
        let error = cancelled.with_current(client.execute(fixture.command(None), None)).await.unwrap_err();
        assert!(matches!(error, Error::Aborted));
        assert!(fixture.receiver.try_recv().is_err());
        assert!(!fixture.output.shared.terminals.has_pending());
    });
}

#[cfg(unix)]
#[test]
fn unnegotiated_terminals_keep_local_foreground_and_explicit_background_stays_managed() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(false);
        let tools = ToolRegistry::from_tools(vec![Box::new(BashTool::new(fixture.root.path()).with_mediation(Some(crate::config::BashSettings {
            persistent: Some(false), ..Default::default()
        })))]);
        bind(&tools, Some(&fixture.permissions), "editor-session", fixture.root.path()).unwrap();
        let output = tools.get("bash").unwrap().execute("local", json!({"command":"printf local", "background":false}), None).await.unwrap();
        assert!(!output.is_error);
        assert!(text(&output).contains("local"));
        assert!(fixture.receiver.try_recv().is_err());

        let editor = Fixture::new(true);
        let output = editor.tools.get("bash").unwrap().execute("background", json!({"command":"printf background", "background":true}), None).await.unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["schema"], crate::jobs::JOB_SCHEMA);
        let result = editor.tools.get("jobs").unwrap().execute("wait", json!({"action":"wait", "jobId":details["id"], "timeoutMs":5000}), None).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.details.as_ref().unwrap()["exitCode"], 0);
        assert!(editor.receiver.try_recv().is_err());
        assert!(!editor.output.shared.terminals.has_pending());
    });
}

struct BashProvider(AtomicU64);

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::provider::Provider for BashProvider {
    fn name(&self) -> &str { "terminal-permission-test" }
    fn api(&self) -> &str { "terminal-permission-test" }
    fn model_id(&self) -> &str { "terminal-permission-test" }
    async fn stream(
        &self, _context: &crate::provider::Context<'_>, _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let partial = AssistantMessage {
            provider: self.name().into(), model: self.model_id().into(), api: self.api().into(),
            ..AssistantMessage::default()
        };
        let mut message = partial.clone();
        message.stop_reason = if first { StopReason::ToolUse } else { StopReason::Stop };
        message.content = if first {
            vec![serde_json::from_value(json!({"type":"toolCall", "id":"editor-bash", "name":"bash", "arguments":{"command":"printf changed > private.txt"}})).unwrap()]
        } else {
            vec![ContentBlock::Text(TextContent::new("Permission denial respected."))]
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Ok(StreamEvent::Done { reason: message.stop_reason, message }),
        ])))
    }
}

#[test]
fn real_agent_permission_denial_starts_no_editor_or_local_process() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true);
        std::fs::write(fixture.root.path().join("private.txt"), "original").unwrap();
        let provider = Arc::new(BashProvider(AtomicU64::new(0)));
        let mut agent = Agent::new(provider.clone(), fixture.tools.clone_shallow(), AgentConfig {
            tool_approval: Some(fixture.permissions.handler_for_session("editor-session".into())),
            ..AgentConfig::default()
        });
        let events = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let mut turn = Box::pin(agent.run("Run the command", move |event| observed.lock().unwrap().push(event)));
        let permission = fixture.await_frame(turn.as_mut(), "session/request_permission").await;
        assert!(route_permission_response(
            &json!({"jsonrpc":"2.0", "id":permission["id"], "result":{"outcome":{"outcome":"selected", "optionId":ACP_PERMISSION_REJECT_ONCE}}}),
            &fixture.permissions.pending, &AgentCx::for_current_or_request(),
        ));
        turn.await.unwrap();
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            AgentEvent::ToolExecutionEnd { result, is_error:true, .. }
                if result.details.as_ref().is_some_and(|details| details["schema"] == "pi.tool.approval_denied.v1")
        )));
        assert_eq!(provider.0.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read_to_string(fixture.root.path().join("private.txt")).unwrap(), "original");
        assert!(fixture.receiver.try_recv().is_err());
        assert!(!fixture.output.shared.terminals.has_pending());
    });
}
