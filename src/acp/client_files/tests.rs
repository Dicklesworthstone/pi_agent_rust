//! Real native tools and Agent approval gates against the editor broker.
use super::*;
use crate::agent::{Agent, AgentConfig};
use crate::model::{AssistantMessage, StopReason, StreamEvent, TextContent};
use crate::tools::{EditTool, HashlineEditTool, ReadTool, ToolOutput, WriteTool};
use std::cell::RefCell;
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
    fn new(read: bool, write: bool) -> Self {
        let root = tempfile::tempdir().expect("workspace");
        let (sender, receiver) = sync_channel(128);
        let output = AcpOutput::new(sender);
        output
            .shared
            .files
            .negotiate(&json!({"clientCapabilities":{"fs":{
                "readTextFile":read, "writeTextFile":write
            }}}));
        let permissions = AcpPermissionClient {
            out_tx: output.clone(),
            pending: Arc::clone(&output.shared.pending_permissions),
            request_counter: Arc::new(AtomicU64::new(0)),
            timeout: Duration::from_secs(5),
            cx: AgentCx::for_current_or_request(),
        };
        let tools = ToolRegistry::from_tools(vec![
            Box::new(ReadTool::new(root.path())),
            Box::new(WriteTool::new(root.path())),
            Box::new(EditTool::new(root.path())),
            Box::new(HashlineEditTool::new(root.path())),
        ]);
        bind(
            &tools,
            Some(&permissions),
            "editor-session",
            root.path(),
            None,
        )
        .unwrap();
        let (abort, signal) = AbortHandle::new();
        let prompt = output.begin_prompt("editor-session".into(), abort).unwrap();
        Self {
            root,
            tools,
            output,
            receiver,
            permissions,
            prompt: Some(prompt),
            abort: signal,
            frames: RefCell::new(Vec::new()),
        }
    }

    fn frame(&self, method: &str) -> Value {
        let frame: Value =
            serde_json::from_str(&self.receiver.try_recv().expect("outbound request"))
                .expect("JSON-RPC request");
        assert_eq!(frame["method"], method);
        assert_eq!(frame["params"]["sessionId"], "editor-session");
        self.frames.borrow_mut().push(frame.clone());
        frame
    }

    fn reply(&self, frame: &Value, result: Value) {
        assert!(self.output.shared.files.route_response(
            &json!({"jsonrpc":"2.0", "id":frame["id"], "result":result}),
            &AgentCx::for_current_or_request(),
        ));
    }

    async fn await_frame<F: std::future::Future + ?Sized>(
        &self,
        mut pending: std::pin::Pin<&mut F>,
        method: &str,
    ) -> Value {
        timeout(
            wall_now(),
            Duration::from_secs(5),
            Box::pin(futures::future::poll_fn(|cx| {
                assert!(
                    pending.as_mut().poll(cx).is_pending(),
                    "operation completed before its expected request"
                );
                match self.receiver.try_recv() {
                    Ok(raw) => std::task::Poll::Ready(raw),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                    Err(error) => panic!("editor output closed: {error}"),
                }
            })),
        )
        .await
        .map(|raw| {
            let frame: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(frame["method"], method);
            assert_eq!(frame["params"]["sessionId"], "editor-session");
            self.frames.borrow_mut().push(frame.clone());
            frame
        })
        .expect("editor request deadline")
    }

    async fn tool(
        &self,
        name: &str,
        arguments: Value,
        replies: Vec<(&str, Value)>,
    ) -> Result<ToolOutput> {
        let mut pending = Box::pin(self.tools.get(name).expect("native tool").execute(
            "editor-tool",
            arguments,
            None,
        ));
        for (method, result) in replies {
            let frame = self.await_frame(pending.as_mut(), method).await;
            assert_eq!(
                Path::new(frame["params"]["path"].as_str().unwrap()).parent(),
                Some(self.root.path())
            );
            self.reply(&frame, result);
        }
        pending.await
    }
}

fn text(output: &ToolOutput) -> &str {
    match output.content.as_slice() {
        [ContentBlock::Text(text)] => &text.text,
        _ => panic!("expected one text output"),
    }
}

#[test]
fn native_read_uses_unsaved_buffers_normalized_lines_and_current_hashes() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        let source = "\u{feff}one\r\ntwo\rthree\n";
        let output = fixture
            .tool(
                "read",
                json!({"path":"new.txt", "hashline":true}),
                vec![("fs/read_text_file", json!({"content":source}))],
            )
            .await
            .unwrap();
        assert!(!fixture.root.path().join("new.txt").exists());
        let lines = text(&output).lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("\u{feff}one"));
        assert!(lines[1].ends_with(":two"));
        assert!(lines[2].ends_with(":three"));
        let first_anchor = lines[0].split_once(':').unwrap().0;
        // The native hashline transformer must accept the BOM-bearing tag
        // produced from the unsaved, normalized source.
        let changed = "\u{feff}ONE\r\ntwo\r\nthree\r\n";
        let output = fixture
            .tool(
                "hashline_edit",
                json!({"path":"new.txt", "edits":[{
                    "op":"replace", "pos":first_anchor, "lines":["ONE"]
                }]}),
                vec![
                    ("fs/read_text_file", json!({"content":source})),
                    ("fs/read_text_file", json!({"content":source})),
                    ("fs/write_text_file", json!({})),
                    ("fs/read_text_file", json!({"content":changed})),
                ],
            )
            .await
            .unwrap();
        assert_eq!(output.details.as_ref().unwrap()["destination"], "editor");
        assert_eq!(output.details.as_ref().unwrap()["verified"], true);
        let frames = fixture.frames.borrow();
        let write = frames
            .iter()
            .find(|frame| frame["method"] == "fs/write_text_file")
            .unwrap();
        assert!(
            write["params"]["content"]
                .as_str()
                .unwrap()
                .starts_with("\u{feff}ONE")
        );
        assert!(!fixture.root.path().join("new.txt").exists());
    });
}

#[test]
fn editor_read_windows_honor_explicit_limits_and_empty_offsets() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, false);
        let output = fixture
            .tool(
                "read",
                json!({"path":"notes.txt", "offset":2, "limit":1}),
                vec![("fs/read_text_file", json!({"content":"one\rtwo\rthree"}))],
            )
            .await
            .unwrap();
        assert!(text(&output).starts_with("    2→two\n"));
        assert!(text(&output).contains("offset=3"));
        let many = "x\n".repeat(2100);
        let output = fixture
            .tool(
                "read",
                json!({"path":"notes.txt", "limit":2100}),
                vec![("fs/read_text_file", json!({"content":many}))],
            )
            .await
            .unwrap();
        assert_eq!(output.details.unwrap()["outputLines"], 2100);
        assert!(
            fixture
                .tool(
                    "read",
                    json!({"path":"empty.txt", "offset":1}),
                    vec![("fs/read_text_file", json!({"content":""})),]
                )
                .await
                .is_err()
        );
        let output = fixture
            .tool(
                "read",
                json!({"path":"empty.txt", "offset":0}),
                vec![("fs/read_text_file", json!({"content":""}))],
            )
            .await
            .unwrap();
        assert_eq!(text(&output), "");
    });
}

#[test]
fn native_edit_and_write_target_editor_and_report_formatter_readback() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        std::fs::write(fixture.root.path().join("code.rs"), "stale disk").unwrap();
        let output = fixture
            .tool(
                "edit",
                json!({"path":"code.rs", "oldText":"unsaved", "newText":"changed"}),
                vec![
                    ("fs/read_text_file", json!({"content":"unsaved\n"})),
                    ("fs/read_text_file", json!({"content":"unsaved\n"})),
                    (
                        "fs/write_text_file",
                        json!({"untrustedMetadata":"must not be copied"}),
                    ),
                    ("fs/read_text_file", json!({"content":"CHANGED\n"})),
                ],
            )
            .await
            .unwrap();
        let details = output.details.as_ref().unwrap();
        assert_eq!(details["editorTransformed"], true);
        assert!(details["diff"].as_str().unwrap().contains("CHANGED"));
        assert!(!details["diff"].as_str().unwrap().contains("+changed"));
        assert_eq!(fixture.frames.borrow()[2]["params"]["content"], "changed\n");
        assert_eq!(
            std::fs::read_to_string(fixture.root.path().join("code.rs")).unwrap(),
            "stale disk"
        );
        let output = fixture
            .tool(
                "write",
                json!({"path":"new.txt", "content":"requested"}),
                vec![
                    ("fs/write_text_file", json!({})),
                    ("fs/read_text_file", json!({"content":"formatted"})),
                ],
            )
            .await
            .unwrap();
        assert_eq!(output.details.unwrap()["editorTransformed"], true);
        assert!(!fixture.root.path().join("new.txt").exists());
    });
}

#[test]
fn editor_errors_and_concurrent_changes_never_fall_back_to_stale_disk() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        std::fs::write(fixture.root.path().join("file.txt"), "disk secret").unwrap();
        assert!(
            fixture
                .tool(
                    "read",
                    json!({"path":"file.txt"}),
                    vec![("fs/read_text_file", json!({"content":7})),]
                )
                .await
                .is_err()
        );
        let error = fixture
            .tool(
                "edit",
                json!({"path":"file.txt", "oldText":"before", "newText":"after"}),
                vec![
                    ("fs/read_text_file", json!({"content":"before"})),
                    (
                        "fs/read_text_file",
                        json!({"content":"concurrent editor change"}),
                    ),
                ],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed while preparing"));
        assert!(
            fixture
                .frames
                .borrow()
                .iter()
                .all(|frame| frame["method"] == "fs/read_text_file")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.path().join("file.txt")).unwrap(),
            "disk secret"
        );
    });
}

#[test]
fn absent_capabilities_stay_local_and_read_only_unsaved_edits_cannot_overwrite_disk() {
    asupersync::test_utils::run_test(|| async {
        let local = Fixture::new(false, false);
        local
            .tools
            .get("write")
            .unwrap()
            .execute("local", json!({"path":"file.txt", "content":"local"}), None)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(local.root.path().join("file.txt")).unwrap(),
            "local"
        );
        assert!(local.receiver.try_recv().is_err());
        let readonly = Fixture::new(true, false);
        std::fs::write(readonly.root.path().join("file.txt"), "saved").unwrap();
        assert!(
            readonly
                .tool(
                    "edit",
                    json!({"path":"file.txt", "oldText":"unsaved", "newText":"changed"}),
                    vec![
                        ("fs/read_text_file", json!({"content":"unsaved"})),
                        ("fs/read_text_file", json!({"content":"unsaved"})),
                    ]
                )
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(readonly.root.path().join("file.txt")).unwrap(),
            "saved"
        );
        assert!(readonly.receiver.try_recv().is_err());
        let output = readonly
            .tool(
                "edit",
                json!({"path":"file.txt", "oldText":"saved", "newText":"changed"}),
                vec![
                    ("fs/read_text_file", json!({"content":"saved"})),
                    ("fs/read_text_file", json!({"content":"saved"})),
                ],
            )
            .await
            .unwrap();
        assert_eq!(output.details.unwrap()["destination"], "disk");
        assert_eq!(
            std::fs::read_to_string(readonly.root.path().join("file.txt")).unwrap(),
            "changed"
        );
    });
}

#[test]
fn write_only_acknowledgment_does_not_claim_verified_bytes() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(false, true);
        let output = fixture
            .tool(
                "write",
                json!({"path":"file.txt", "content":"requested"}),
                vec![("fs/write_text_file", json!({}))],
            )
            .await
            .unwrap();
        let details = output.details.as_ref().unwrap();
        assert_eq!(details["destination"], "editor");
        assert_eq!(details["verified"], false);
        assert!(details.get("diff").is_none());
        assert!(text(&output).contains("not rolled back"));
        assert!(!fixture.abort.is_aborted());
        assert!(!fixture.root.path().join("file.txt").exists());
    });
}

#[test]
fn path_confinement_precedes_any_editor_request() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("private.txt");
        std::fs::write(&path, "private").unwrap();
        assert!(
            fixture
                .tools
                .get("read")
                .unwrap()
                .execute("outside", json!({"path":path}), None)
                .await
                .is_err()
        );
        assert!(
            fixture
                .tools
                .get("write")
                .unwrap()
                .execute("outside", json!({"path":path, "content":"overwrite"}), None)
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, fixture.root.path().join("link.txt")).unwrap();
            assert!(
                fixture
                    .tools
                    .get("edit")
                    .unwrap()
                    .execute(
                        "symlink",
                        json!({"path":"link.txt", "oldText":"private", "newText":"overwrite"}),
                        None
                    )
                    .await
                    .is_err()
            );
        }
        assert!(fixture.receiver.try_recv().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "private");
    });
}

#[test]
fn cancelled_unpublished_writes_are_safe_and_published_writes_fence_the_owning_session() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        fixture.output.abort_prompt("editor-session");
        assert!(
            fixture
                .tools
                .get("write")
                .unwrap()
                .execute(
                    "cancelled",
                    json!({"path":"file.txt", "content":"new"}),
                    None
                )
                .await
                .is_err()
        );
        assert!(fixture.receiver.try_recv().is_err());
        assert!(!fixture.output.shared.files.has_write("editor-session"));

        let mut fixture = Fixture::new(true, true);
        let mut pending = Box::pin(fixture.tools.get("write").unwrap().execute(
            "published",
            json!({"path":"file.txt", "content":"new"}),
            None,
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let request = fixture.frame("fs/write_text_file");
        drop(pending);
        assert!(fixture.abort.is_aborted());
        assert!(fixture.output.shared.files.has_write("editor-session"));
        let notice = fixture.frame("session/update");
        assert!(notice.to_string().contains("not rolled back"));
        drop(fixture.prompt.take());
        assert!(
            fixture
                .output
                .begin_prompt("editor-session".into(), AbortHandle::new().0)
                .is_err()
        );
        let other = fixture
            .output
            .begin_prompt("another-session".into(), AbortHandle::new().0)
            .unwrap();
        let cx = AgentCx::for_current_or_request();
        assert!(!fixture.output.shared.files.route_response(
            &json!({"jsonrpc":"2.0", "id":"stale-write", "result":{}}),
            &cx
        ));
        fixture.reply(&request, json!([]));
        assert!(fixture.output.shared.files.has_write("editor-session"));
        fixture.reply(&request, json!({}));
        assert!(!fixture.output.shared.files.has_write("editor-session"));
        drop(
            fixture
                .output
                .begin_prompt("editor-session".into(), AbortHandle::new().0)
                .unwrap(),
        );
        drop(other);
    });
}

#[test]
fn cancelled_reads_ignore_late_replies_and_oversized_responses_fail_closed() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        let mut pending = Box::pin(fixture.tools.get("read").unwrap().execute(
            "cancelled",
            json!({"path":"file.txt"}),
            None,
        ));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let old = fixture.frame("fs/read_text_file");
        drop(pending);
        let cancelled: Value = serde_json::from_str(&fixture.receiver.try_recv().unwrap()).unwrap();
        assert_eq!(cancelled["method"], "$/cancel_request");
        assert_eq!(cancelled["params"]["requestId"], old["id"]);
        let mut current = Box::pin(fixture.tools.get("read").unwrap().execute(
            "current",
            json!({"path":"file.txt"}),
            None,
        ));
        assert!(futures::poll!(current.as_mut()).is_pending());
        let request = fixture.frame("fs/read_text_file");
        assert_ne!(old["id"], request["id"]);
        assert!(!fixture.output.shared.files.route_response(
            &json!({"jsonrpc":"2.0", "id":old["id"], "result":{"content":"stale"}}),
            &AgentCx::for_current_or_request()
        ));
        fixture.reply(&request, json!({"content":"x".repeat(MAX_TEXT_BYTES + 1)}));
        assert!(current.await.is_err());
        assert!(
            fixture
                .output
                .shared
                .files
                .state
                .lock()
                .unwrap()
                .pending
                .is_empty()
        );
        assert!(!fixture.abort.is_aborted());
    });
}

struct WriteProvider(AtomicU64);

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::provider::Provider for WriteProvider {
    fn name(&self) -> &str {
        "editor-permission-test"
    }
    fn api(&self) -> &str {
        "editor-permission-test"
    }
    fn model_id(&self) -> &str {
        "editor-permission-test"
    }
    async fn stream(
        &self,
        _context: &crate::provider::Context<'_>,
        _options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let partial = AssistantMessage {
            provider: self.name().into(),
            model: self.model_id().into(),
            api: self.api().into(),
            ..AssistantMessage::default()
        };
        let mut message = partial.clone();
        message.stop_reason = if first {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        message.content = if first {
            vec![serde_json::from_value(json!({"type":"toolCall", "id":"editor-write", "name":"write", "arguments":{"path":"private.txt", "content":"changed"}})).unwrap()]
        } else {
            vec![ContentBlock::Text(TextContent::new(
                "Permission denial respected.",
            ))]
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Ok(StreamEvent::Done {
                reason: message.stop_reason,
                message,
            }),
        ])))
    }
}

#[test]
fn real_agent_permission_denial_publishes_no_filesystem_request() {
    asupersync::test_utils::run_test(|| async {
        let fixture = Fixture::new(true, true);
        std::fs::write(fixture.root.path().join("private.txt"), "original").unwrap();
        let provider = Arc::new(WriteProvider(AtomicU64::new(0)));
        let mut agent = Agent::new(
            provider.clone(),
            fixture.tools.clone_shallow(),
            AgentConfig {
                tool_approval: Some(
                    fixture
                        .permissions
                        .handler_for_session("editor-session".into()),
                ),
                ..AgentConfig::default()
            },
        );
        let events = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let mut turn = Box::pin(agent.run("Update the file", move |event| {
            observed.lock().unwrap().push(event)
        }));
        let permission = fixture
            .await_frame(turn.as_mut(), "session/request_permission")
            .await;
        assert!(route_permission_response(
            &json!({"jsonrpc":"2.0", "id":permission["id"], "result":{
                "outcome":{"outcome":"selected", "optionId":ACP_PERMISSION_REJECT_ONCE}
            }}),
            &fixture.permissions.pending,
            &AgentCx::for_current_or_request()
        ));
        turn.await.unwrap();
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            AgentEvent::ToolExecutionEnd { result, is_error:true, .. }
                if result.details.as_ref().is_some_and(|details| details["schema"] == "pi.tool.approval_denied.v1")
        )));
        assert_eq!(provider.0.load(Ordering::SeqCst), 2);
        assert!(fixture.receiver.try_recv().is_err());
        assert!(
            fixture
                .output
                .shared
                .files
                .state
                .lock()
                .unwrap()
                .pending
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.path().join("private.txt")).unwrap(),
            "original"
        );
    });
}
