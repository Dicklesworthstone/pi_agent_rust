use super::*;

fn fixture() -> (tempfile::TempDir, AgentHubRegistry, String, String) {
    let dir = tempfile::tempdir().expect("hub directory");
    let mut hub = AgentHubRegistry::default();
    hub.set_dir_for_tests(dir.path().to_path_buf());
    let first = hub.register("first", "first task").expect("first");
    let second = hub.register("second", "second task").expect("second");
    hub.mark_running(&first.id, 11);
    hub.mark_running(&second.id, 12);
    (dir, hub, first.id, second.id)
}

#[test]
fn replies_are_filtered_by_peer_and_exclusive_cursor_without_consumption() {
    let (_dir, mut hub, first, second) = fixture();
    let question = hub.bus_send(&first, "parent", "question").unwrap();
    hub.bus_send("parent", &second, "unrelated").unwrap();
    let pending = hub.wait_snapshot("parent", &first, question.seq).unwrap();
    assert_eq!(pending.status, InboxWaitStatus::Pending);
    assert!(pending.messages.is_empty());
    let answer = hub.bus_send("parent", &first, "answer").unwrap();
    let result = hub
        .wait_snapshot("parent", &first, pending.next_cursor)
        .unwrap();
    assert_eq!(result.status, InboxWaitStatus::Messages);
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].seq, answer.seq);
    assert_eq!(result.next_cursor, answer.seq);
    assert_eq!(hub.inbox("parent").len(), 2);
    assert_eq!(
        hub.wait_snapshot("parent", &first, result.next_cursor)
            .unwrap()
            .status,
        InboxWaitStatus::Pending
    );
}

#[test]
fn retained_reply_wins_over_peer_exit_and_missing_reply_finishes() {
    let (_dir, mut hub, first, _) = fixture();
    let answer = hub.bus_send("parent", &first, "final answer").unwrap();
    hub.settle(&first, ChildStatus::Done);
    let reply = hub.wait_snapshot("parent", &first, 0).unwrap();
    assert_eq!(reply.status, InboxWaitStatus::Messages);
    assert_eq!(reply.peer_status, Some(ChildStatus::Done));
    let stopped = hub.wait_snapshot("parent", &first, answer.seq).unwrap();
    assert_eq!(stopped.status, InboxWaitStatus::PeerStopped);
}

#[test]
fn sequence_gaps_do_not_mean_loss_but_actual_eviction_does() {
    let (_dir, mut hub, first, second) = fixture();
    for _ in 0..5 {
        hub.bus_send(&first, "parent", "another inbox").unwrap();
    }
    hub.bus_send("parent", &second, "different sender").unwrap();
    let intact = hub.wait_snapshot("parent", &first, 0).unwrap();
    assert_eq!(intact.status, InboxWaitStatus::Pending);
    assert_eq!(intact.dropped_through_cursor, 0);
    let lost = hub.bus_send("parent", &first, "evicted answer").unwrap();
    for _ in 0..super::super::MAX_QUEUE_PER_CHILD {
        hub.bus_send("parent", &second, "later unrelated message")
            .unwrap();
    }
    let result = hub.wait_snapshot("parent", &first, 0).unwrap();
    assert_eq!(result.status, InboxWaitStatus::HistoryLost);
    assert!(result.dropped_through_cursor >= lost.seq);
    assert!(result.messages.is_empty());
    let acknowledged = hub
        .wait_snapshot("parent", &first, result.next_cursor)
        .unwrap();
    assert_eq!(acknowledged.status, InboxWaitStatus::Pending);
}

#[test]
fn byte_bounded_pages_keep_every_matching_reply_in_order() {
    let (_dir, mut hub, first, second) = fixture();
    let body = "界".repeat(15_000);
    let mut expected = Vec::new();
    for _ in 0..4 {
        expected.push(hub.bus_send("parent", &first, &body).unwrap().seq);
        hub.bus_send("parent", &second, "skip me").unwrap();
    }
    let page = hub.wait_snapshot("parent", &first, 0).unwrap();
    assert!(page.has_more);
    assert_eq!(page.messages.len(), 2);
    let tail = hub
        .wait_snapshot("parent", &first, page.next_cursor)
        .unwrap();
    assert!(!tail.has_more);
    let actual: Vec<_> = page
        .messages
        .iter()
        .chain(&tail.messages)
        .map(|m| m.seq)
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(hub.inbox("parent").len(), 8);
}

#[test]
fn own_inbox_and_peer_identity_are_required() {
    let (_dir, hub, first, _) = fixture();
    assert!(
        hub.wait_snapshot(&first, &first, 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_INPUT")
    );
    assert!(
        hub.wait_snapshot("parent", "missing", 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_PEER")
    );
    assert!(
        hub.wait_snapshot("missing", &first, 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_RECIPIENT")
    );
    assert_eq!(
        hub.wait_snapshot(&first, "parent", 0).unwrap().status,
        InboxWaitStatus::Pending
    );
}

#[test]
fn wait_budget_is_finite_and_cancellation_wins() {
    let owner = AgentCx::for_testing_with_io();
    for milliseconds in [0, MAX_WAIT_MS + 1, u64::MAX] {
        assert!(WaitBudget::new(&owner, milliseconds).is_err());
    }
    let mut budget = WaitBudget::new(&owner, 50).unwrap();
    budget.wall_start = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    assert!(budget.remaining(&owner).unwrap().is_zero());
    owner.set_cancel_requested(true);
    assert!(
        budget
            .remaining(&owner)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_CANCELLED")
    );
}

#[test]
fn only_overall_ipc_timeout_with_a_complete_prior_probe_becomes_timed_out() {
    let (_dir, hub, first, _) = fixture();
    let pending = hub.wait_snapshot("parent", &first, 0).unwrap();
    let timeout = || Error::tool("hub", "PI_HUB_IPC_TIMEOUT: exchange expired");
    let result = expired_probe(timeout(), Duration::ZERO, Some(pending.clone())).unwrap();
    assert_eq!(result.status, InboxWaitStatus::TimedOut);
    assert!(expired_probe(timeout(), Duration::from_secs(1), Some(pending.clone())).is_err());
    assert!(expired_probe(timeout(), Duration::ZERO, None).is_err());
    for code in [
        "CANCELLED",
        "AUTH",
        "REVOKED",
        "CONFIG",
        "UNAVAILABLE",
        "READ",
    ] {
        let error = Error::tool("hub", format!("PI_HUB_IPC_{code}: failed"));
        assert!(expired_probe(error, Duration::ZERO, Some(pending.clone())).is_err());
    }
    assert!(has_code(
        &Error::tool("hub", "Tool error: hub: PI_HUB_IPC_BUSY: try later"),
        "PI_HUB_IPC_BUSY:"
    ));
    assert!(!has_code(
        &Error::tool("hub", "PI_HUB_IPC_UNAVAILABLE: poisoned"),
        "PI_HUB_IPC_BUSY:"
    ));
}
