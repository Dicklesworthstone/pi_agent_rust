//! Isolation at live session, invocation and native tool boundaries.

use super::{AgentHubRegistry, ChildStatus, SharedHub, current_registry, enter_registry, with_registry};
use crate::jobs::JobSessionScope;
use crate::tools::{HubTool, Tool};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

fn session_hub(scope: &JobSessionScope, dir: &std::path::Path) -> SharedHub {
    std::fs::create_dir_all(dir).expect("hub directory");
    let hub = scope.agent_hub_for_session("saved-session").expect("hub");
    hub.lock().unwrap().set_dir_for_tests(dir.to_path_buf());
    hub
}

fn assert_current(expected: &SharedHub) {
    assert!(Arc::ptr_eq(expected, &current_registry()));
}

struct PollProbe {
    hub: SharedHub,
    pending: bool,
    panic_on_poll: bool,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Future for PollProbe {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        assert_current(&self.hub);
        assert!(!self.panic_on_poll, "deliberate poll panic");
        if self.pending {
            self.pending = false;
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl Drop for PollProbe {
    fn drop(&mut self) {
        assert_current(&self.hub);
        self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn interleaved_polls_unpolled_drop_and_unwind_restore_the_parent_hub() {
    let outer = Arc::new(Mutex::new(AgentHubRegistry::default()));
    let first = Arc::new(Mutex::new(AgentHubRegistry::default()));
    let second = Arc::new(Mutex::new(AgentHubRegistry::default()));
    let _outer = enter_registry(Arc::clone(&outer));
    let first_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut left = Box::pin(with_registry(
        Arc::clone(&first),
        PollProbe {
            hub: Arc::clone(&first),
            pending: true,
            panic_on_poll: false,
            dropped: Arc::clone(&first_dropped),
        },
    ));
    let mut right = Box::pin(with_registry(
        Arc::clone(&second),
        PollProbe {
            hub: Arc::clone(&second),
            pending: true,
            panic_on_poll: false,
            dropped: Arc::clone(&second_dropped),
        },
    ));
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(left.as_mut().poll(&mut cx).is_pending());
    assert_current(&outer);
    assert!(right.as_mut().poll(&mut cx).is_pending());
    assert_current(&outer);
    assert!(left.as_mut().poll(&mut cx).is_ready());
    drop(left);
    drop(right);
    assert!(first_dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(second_dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_current(&outer);

    let unpolled_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    drop(with_registry(
        Arc::clone(&first),
        PollProbe {
            hub: Arc::clone(&first),
            pending: true,
            panic_on_poll: false,
            dropped: Arc::clone(&unpolled_dropped),
        },
    ));
    assert!(unpolled_dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_current(&outer);

    let panic_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut panicking = Box::pin(with_registry(
        Arc::clone(&second),
        PollProbe {
            hub: second,
            pending: false,
            panic_on_poll: true,
            dropped: Arc::clone(&panic_dropped),
        },
    ));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = panicking.as_mut().poll(&mut cx);
        }))
        .is_err()
    );
    assert_current(&outer);
    drop(panicking);
    assert!(panic_dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_current(&outer);
}

#[test]
fn same_saved_session_isolated_by_live_scope_and_clones_retain_authority() {
    let first = JobSessionScope::fixed("saved-session");
    let second = JobSessionScope::fixed("saved-session");
    let first_hub = first.agent_hub_for_session("saved-session").unwrap();
    let clone_hub = first.clone().agent_hub_for_session("saved-session").unwrap();
    let second_hub = second.agent_hub_for_session("saved-session").unwrap();
    let switched_hub = first.agent_hub_for_session("other-session").unwrap();
    assert!(Arc::ptr_eq(&first_hub, &clone_hub));
    assert!(!Arc::ptr_eq(&first_hub, &second_hub));
    assert!(!Arc::ptr_eq(&first_hub, &switched_hub));
    assert!(first.agent_hub_for_session(" ").is_err());

    let retained = Arc::clone(&first_hub);
    futures::executor::block_on(with_registry(Arc::clone(&first_hub), async {
        first.bind(Arc::new(|| Box::pin(async { Some("other-session".to_string()) })));
        assert_eq!(first.session_id().await.unwrap(), "other-session");
        assert_eq!(first.invocation_session_id().await.unwrap(), "saved-session");
        assert!(Arc::ptr_eq(&retained, &first.invocation_agent_hub().await.unwrap()));
    }));
    assert!(Arc::ptr_eq(
        &switched_hub,
        &futures::executor::block_on(first.agent_hub()).unwrap()
    ));
}

fn call(tool: &HubTool, input: Value) -> crate::tools::ToolOutput {
    futures::executor::block_on(tool.execute("hub-isolation", input, None)).expect("tool result")
}

#[test]
fn bound_hub_tools_isolate_roster_controls_transcripts_and_parent_inboxes() {
    let temp = tempfile::tempdir().unwrap();
    let first_scope = JobSessionScope::fixed("saved-session");
    let second_scope = JobSessionScope::fixed("saved-session");
    let first = session_hub(&first_scope, &temp.path().join("first"));
    let second = session_hub(&second_scope, &temp.path().join("second"));
    let (first_id, foreign_id) = {
        let mut hub = first.lock().unwrap();
        let shared = hub.register("scout", "first private assignment").unwrap();
        let foreign = hub.register("first-only", "foreign private assignment").unwrap();
        hub.append_transcript(&shared.id, r#"{"type":"text","text":"first private transcript"}"#);
        for index in 0..=super::MAX_QUEUE_PER_CHILD {
            hub.bus_send("parent", &shared.id, &format!("first reply {index}")).unwrap();
        }
        (shared.id, foreign.id)
    };
    let second_id = {
        let mut hub = second.lock().unwrap();
        let shared = hub.register("scout", "second private assignment").unwrap();
        hub.append_transcript(&shared.id, r#"{"type":"text","text":"second private transcript"}"#);
        hub.bus_send("parent", &shared.id, "second reply").unwrap();
        shared.id
    };
    assert_eq!(first_id, second_id, "display ids may collide across parents");
    let mut first_tool = HubTool::new(temp.path());
    first_tool.bind_job_session_scope(first_scope);
    let mut second_tool = HubTool::new(temp.path());
    second_tool.bind_job_session_scope(second_scope);

    let roster = call(&second_tool, json!({"op":"agent","action":"roster"}));
    let children = &roster.details.as_ref().unwrap()["children"];
    assert_eq!(children.as_array().unwrap().len(), 1);
    assert_eq!(children[0]["task"], "second private assignment");
    let transcript = call(
        &second_tool,
        json!({"op":"agent","action":"transcript","name":second_id}),
    );
    let serialized = serde_json::to_string(&transcript).unwrap();
    assert!(serialized.contains("second private transcript"));
    assert!(!serialized.contains("first private transcript"));

    for action in ["transcript", "steer", "send", "kill", "revive"] {
        let output = call(
            &second_tool,
            json!({"op":"agent","action":action,"name":foreign_id,"text":"unauthorized"}),
        );
        assert!(output.is_error, "foreign {action} must fail");
    }
    assert_eq!(first.lock().unwrap().get(&foreign_id).unwrap().status, ChildStatus::Starting);
    assert!(first.lock().unwrap().inbox(&foreign_id).is_empty());
    let inbox = call(&second_tool, json!({"op":"agent","action":"inbox","name":"parent"}));
    assert_eq!(inbox.details.as_ref().unwrap()["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox.details.as_ref().unwrap()["messages"][0]["body"], "second reply");
    assert_eq!(
        call(&second_tool, json!({"op":"agent","action":"inbox","name":"parent"})).details,
        inbox.details,
        "observing an inbox must not consume it",
    );
    assert!(first.lock().unwrap().wait_snapshot("parent", None, 0).unwrap().dropped_through_cursor > 0);
    assert_eq!(second.lock().unwrap().wait_snapshot("parent", None, 0).unwrap().dropped_through_cursor, 0);

    let killed = call(&first_tool, json!({"op":"agent","action":"kill","name":first_id}));
    assert!(!killed.is_error);
    assert_eq!(first.lock().unwrap().get(&first_id).unwrap().status, ChildStatus::Killed);
    assert_eq!(second.lock().unwrap().get(&second_id).unwrap().status, ChildStatus::Starting);
}

#[cfg(unix)]
#[test]
fn native_launch_failure_and_revival_stay_with_the_bound_live_parent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let global = temp.path().join("global");
    std::fs::create_dir_all(global.join("agents")).unwrap();
    std::fs::write(
        global.join("agents/scout.md"),
        "---\nname: scout\ndescription: session ownership fixture\n---\nReturn the child result.\n",
    )
    .unwrap();
    let success = temp.path().join("success.sh");
    std::fs::write(
        &success,
        r#"#!/bin/sh
printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"owned native result"}]}]}\n'
"#,
    )
    .unwrap();
    let failure = temp.path().join("failure.sh");
    std::fs::write(&failure, "#!/bin/sh\nexit 7\n").unwrap();
    for path in [&success, &failure] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let first_scope = JobSessionScope::fixed("saved-session");
    let second_scope = JobSessionScope::fixed("saved-session");
    let first = session_hub(&first_scope, &temp.path().join("first"));
    let second = session_hub(&second_scope, &temp.path().join("second"));
    let mut first_tool = crate::subagents::SubagentTool::with_paths(
        temp.path().to_path_buf(),
        global.clone(),
        success,
    );
    first_tool.bind_job_session_scope(first_scope.clone());
    let mut second_tool = crate::subagents::SubagentTool::with_paths(
        temp.path().to_path_buf(),
        global,
        failure,
    );
    second_tool.bind_job_session_scope(second_scope.clone());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let completed = runtime.block_on(first_tool.execute(
        "first-native",
        json!({"agent":"scout","task":"first native task"}),
        None,
    )).unwrap();
    assert!(!completed.is_error);
    let failed = runtime.block_on(second_tool.execute(
        "second-native",
        json!({"agent":"scout","task":"second native task"}),
        None,
    )).unwrap();
    assert!(failed.is_error);
    let first_entry = first.lock().unwrap().roster().pop().unwrap();
    let second_entry = second.lock().unwrap().roster().pop().unwrap();
    assert_eq!(first_entry.id, second_entry.id);
    assert_eq!(first_entry.status, ChildStatus::Done);
    assert_eq!(second_entry.status, ChildStatus::Failed);
    assert_ne!(first_entry.transcript_path, second_entry.transcript_path);

    let mut hub_tool = HubTool::new(temp.path());
    hub_tool.bind_job_session_scope(first_scope);
    let revived = runtime.block_on(hub_tool.execute(
        "revive-native",
        json!({"op":"agent","action":"revive","name":first_entry.id}),
        None,
    )).unwrap();
    assert!(!revived.is_error);
    let roster = first.lock().unwrap().roster();
    assert_eq!(roster.len(), 2);
    assert!(roster.iter().any(|entry| entry.revived_from.as_deref() == Some(first_entry.id.as_str())));
    assert_eq!(second.lock().unwrap().roster().len(), 1);
    assert_eq!(second.lock().unwrap().get(&second_entry.id).unwrap().status, ChildStatus::Failed);

    let tan = runtime
        .block_on(first_tool.run_background_tan("bound SDK tangential work"))
        .unwrap();
    assert!(!tan.is_error);
    let tan_id = tan.hub_id.unwrap();
    assert_eq!(
        first.lock().unwrap().get(&tan_id).unwrap().kind,
        super::ChildKind::Tan,
    );
    assert!(second.lock().unwrap().get(&tan_id).is_none());
    assert_eq!(first.lock().unwrap().roster().len(), 3);
}
