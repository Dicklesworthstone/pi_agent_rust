//! Native SDK/provider/HTTP/SSE/tool coverage with only the deadline clock injected.
//! The local peer's finite socket watchdog makes missing cancellation fail rather
//! than leaving an infinite stream in the test runner.

use super::*;
use crate::agent_cx::AgentCx;
use asupersync::time::{TimerDriverHandle, VirtualClock};
use asupersync::types::Time;
use std::sync::atomic::AtomicUsize;

fn deadline() -> (Arc<VirtualClock>, TimerDriverHandle, TurnDeadline) {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
    let limit = TurnDeadline::from_timer(
        &AgentCx::for_request(),
        timer.clone(),
        Duration::from_secs(10),
    )
    .unwrap();
    (clock, timer, limit)
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_stream_timeout_recovers_attachments_into_a_real_tool_turn_exactly_once() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "real recovery read").unwrap();
    let stalled_peer = Peer::new(vec![Reply::Hanging]);
    let recovery_peer = Peer::new(vec![Reply::Tool, Reply::Text, Reply::Text]);
    let mut original = stalled_peer.session(temp.path());
    let mut recovery = recovery_peer.session(temp.path());
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let saw_stream = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&saw_stream);
    let tools = Arc::clone(&tool_calls);
    let (clock, timer, limit) = deadline();
    runtime.block_on(async {
        let interrupted = original
            .prompt_with_control("original timed-out task".to_string(), move |control, event| {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                ) && !observed.swap(true, Ordering::SeqCst)
                {
                    let content = UserContent::Blocks(vec![
                        crate::model::ContentBlock::Text(crate::model::TextContent::new(
                            "inspect recovered image",
                        )),
                        crate::model::ContentBlock::Image(crate::model::ImageContent {
                            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC".to_string(),
                            mime_type: "image/png".to_string(),
                        }),
                    ]);
                    control
                        .follow_up_with_content(&content, "inspect image")
                        .unwrap();
                    clock.advance_to(Time::from_secs(10));
                    let _ = timer.process_timers();
                }
            })
            .unwrap()
            .with_deadline(limit);
        let old = interrupted.control();
        let error = interrupted.await.unwrap_err();
        assert!(error.is_elapsed());
        assert_eq!(
            error.completion().unwrap().as_ref().unwrap().stop_reason,
            StopReason::Aborted,
            "native cancellation, not eventual fixture EOF, must settle the stream"
        );
        assert!(old.snapshot().finished);
        assert_eq!(old.snapshot().pending_follow_up, 1);
        assert_eq!(old.snapshot().handed_to_agent, 0);
        let next = recovery
            .prompt("read note.txt".to_string(), move |event| {
                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                    tools.fetch_add(1, Ordering::SeqCst);
                }
            })
            .unwrap();
        let target = next.control();
        let transferred = old.transfer_pending_to(&target).unwrap();
        assert_eq!(transferred.len(), 1);
        assert_ne!(transferred[0].previous_id, transferred[0].new_id);
        assert!(target.retract(transferred[0].previous_id).is_none());
        assert!(old.transfer_pending_to(&target).unwrap().is_empty());
        assert!(!old.abort(), "an old handle cannot cancel recovered work");
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
        assert!(target.snapshot().finished);
        assert_eq!(target.snapshot().handed_to_agent, 1);
        assert!(target.take_pending().is_empty());
        assert_eq!(old.snapshot().pending_bytes, 0);
    });
    assert!(saw_stream.load(Ordering::SeqCst));
    assert_eq!(lock(&stalled_peer.requests).len(), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    let requests = lock(&recovery_peer.requests);
    assert_eq!(requests.len(), 3);
    assert!(!request_has(&requests[0], "inspect recovered image"));
    assert!(!request_has(&requests[2], "original timed-out task"));
    let parts = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .find(|parts| parts.iter().any(|part| part["type"] == "image_url"))
        .expect("recovered native image reaches the real provider");
    assert_eq!(parts[0]["text"], "inspect recovered image");
    assert!(
        parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    drop(requests);
    let messages = recovery
        .session_mut()
        .session_store()
        .try_lock()
        .unwrap()
        .to_messages_for_current_path();
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count(),
        1
    );
    eprintln!(
        "{}",
        json!({
            "schema": "pi.test.session_deadline_recovery.v1",
            "outcome": "timeout_then_explicit_recovery",
            "original_requests": 1, "recovery_requests": 3,
            "tool_calls": 1, "transferred_inputs": 1,
            "attachments_preserved": true
        })
    );
}

#[test]
fn expired_unpolled_native_prompt_has_no_request_event_or_history_side_effect() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Text]);
    let mut session = peer.session(temp.path());
    let store = session.session_mut().session_store();
    let before = store.try_lock().unwrap().entries.len();
    let events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&events);
    let (clock, _, limit) = deadline();
    runtime.block_on(async {
        let turn = session
            .prompt("expired prompt must not run".to_string(), move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        let control = turn.control();
        control.follow_up("retain intent").unwrap();
        clock.advance_to(Time::from_secs(10));
        let error = turn.with_deadline(limit).await.unwrap_err();
        assert!(error.is_elapsed());
        assert!(error.completion().is_none());
        assert!(control.snapshot().finished);
        assert_eq!(control.take_pending()[0].text, "retain intent");
        assert_eq!(events.load(Ordering::SeqCst), 0);
        assert_eq!(store.try_lock().unwrap().entries.len(), before);
        assert!(lock(&peer.requests).is_empty());
        assert_eq!(
            session
                .prompt("later intended prompt".to_string(), |_| {})
                .unwrap()
                .await
                .unwrap()
                .stop_reason,
            StopReason::Stop
        );
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 1);
    assert!(!request_has(&requests[0], "expired prompt must not run"));
}

#[test]
fn expired_shared_budget_cannot_dispatch_a_native_continuation() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Error, Reply::Text]);
    let mut session = peer.session(temp.path());
    let (clock, _, limit) = deadline();
    runtime.block_on(async {
        let first = session
            .prompt("original prompt".to_string(), |_| {})
            .unwrap()
            .with_deadline(limit.clone())
            .await;
        assert!(first.is_err() || first.unwrap().stop_reason == StopReason::Error);
        let store = session.session_mut().session_store();
        let before = store.try_lock().unwrap().entries.len();
        clock.advance_to(Time::from_secs(10));
        let expired = session.continue_turn(|_| {}).unwrap().with_deadline(limit);
        let control = expired.control();
        let error = expired.await.unwrap_err();
        assert!(error.is_elapsed());
        assert!(error.completion().is_none());
        assert!(control.snapshot().finished);
        assert_eq!(store.try_lock().unwrap().entries.len(), before);
        assert_eq!(lock(&peer.requests).len(), 1);
        // Only a new explicit continuation is allowed to re-enter the provider.
        assert_eq!(
            session
                .continue_turn(|_| {})
                .unwrap()
                .await
                .unwrap()
                .stop_reason,
            StopReason::Stop
        );
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| {
                message["role"] == "user"
                    && message["content"].to_string().contains("original prompt")
            })
            .count(),
        1
    );
}
