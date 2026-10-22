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
    let pending = hub
        .wait_snapshot("parent", Some(&first), question.seq)
        .unwrap();
    assert_eq!(pending.status, InboxWaitStatus::Pending);
    assert!(pending.messages.is_empty());
    let answer = hub.bus_send("parent", &first, "answer").unwrap();
    let result = hub
        .wait_snapshot("parent", Some(&first), pending.next_cursor)
        .unwrap();
    assert_eq!(result.status, InboxWaitStatus::Messages);
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].seq, answer.seq);
    assert_eq!(result.next_cursor, answer.seq);
    assert_eq!(hub.inbox("parent").len(), 2);
    assert_eq!(
        hub.wait_snapshot("parent", Some(&first), result.next_cursor)
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
    let reply = hub.wait_snapshot("parent", Some(&first), 0).unwrap();
    assert_eq!(reply.status, InboxWaitStatus::Messages);
    assert_eq!(reply.peer_status, Some(ChildStatus::Done));
    let stopped = hub
        .wait_snapshot("parent", Some(&first), answer.seq)
        .unwrap();
    assert_eq!(stopped.status, InboxWaitStatus::PeerStopped);
}

#[test]
fn sequence_gaps_do_not_mean_loss_but_actual_eviction_does() {
    let (_dir, mut hub, first, second) = fixture();
    for _ in 0..5 {
        hub.bus_send(&first, "parent", "another inbox").unwrap();
    }
    hub.bus_send("parent", &second, "different sender").unwrap();
    let intact = hub.wait_snapshot("parent", Some(&first), 0).unwrap();
    assert_eq!(intact.status, InboxWaitStatus::Pending);
    assert_eq!(intact.dropped_through_cursor, 0);
    let lost = hub.bus_send("parent", &first, "evicted answer").unwrap();
    for _ in 0..super::super::MAX_QUEUE_PER_CHILD {
        hub.bus_send("parent", &second, "later unrelated message")
            .unwrap();
    }
    let result = hub.wait_snapshot("parent", Some(&first), 0).unwrap();
    assert_eq!(result.status, InboxWaitStatus::HistoryLost);
    assert!(result.dropped_through_cursor >= lost.seq);
    assert!(result.messages.is_empty());
    let acknowledged = hub
        .wait_snapshot("parent", Some(&first), result.next_cursor)
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
    let page = hub.wait_snapshot("parent", Some(&first), 0).unwrap();
    assert!(page.has_more);
    assert_eq!(page.messages.len(), 2);
    let tail = hub
        .wait_snapshot("parent", Some(&first), page.next_cursor)
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
        hub.wait_snapshot(&first, Some(&first), 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_INPUT")
    );
    assert!(
        hub.wait_snapshot("parent", Some("missing"), 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_PEER")
    );
    assert!(
        hub.wait_snapshot("missing", Some(&first), 0)
            .unwrap_err()
            .to_string()
            .contains("PI_HUB_WAIT_RECIPIENT")
    );
    assert_eq!(
        hub.wait_snapshot(&first, Some("parent"), 0).unwrap().status,
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
    let pending = hub.wait_snapshot("parent", Some(&first), 0).unwrap();
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

#[test]
fn automatic_wait_ignores_unasked_agents_and_tracks_only_unanswered_sends() {
    let (_dir, mut hub, first, second) = fixture();
    let idle = hub.wait_snapshot("parent", None, 0).unwrap();
    assert_eq!(idle.status, InboxWaitStatus::NothingPending);
    assert!(idle.peer.is_none());
    assert!(idle.pending_replies.is_empty());

    let first_question = hub.bus_send(&first, "parent", "first question").unwrap();
    let second_question = hub.bus_send(&second, "parent", "second question").unwrap();
    hub.settle(&first, ChildStatus::Done);
    let pending = hub
        .wait_snapshot("parent", None, second_question.seq)
        .unwrap();
    assert_eq!(pending.status, InboxWaitStatus::Pending);
    assert_eq!(pending.pending_replies.len(), 2);
    assert_eq!(pending.pending_replies[0].sent_cursor, first_question.seq);
    assert!(!pending.pending_replies[0].can_reply);
    assert!(pending.pending_replies[1].can_reply);

    let answer = hub.bus_send("parent", &second, "second answer").unwrap();
    let result = hub
        .wait_snapshot("parent", None, second_question.seq)
        .unwrap();
    assert_eq!(result.status, InboxWaitStatus::Messages);
    assert_eq!(result.messages[0].seq, answer.seq);
    assert_eq!(result.pending_replies.len(), 1);
    assert_eq!(result.pending_replies[0].peer, first);
    let finished = hub
        .wait_snapshot("parent", None, result.next_cursor)
        .unwrap();
    assert_eq!(finished.status, InboxWaitStatus::PeerStopped);
    assert_eq!(hub.inbox("parent").len(), 1);
    // The answer itself is a send by the second agent. Only a newer message
    // from its recipient can answer it; reading never fabricates a reply.
    assert_eq!(hub.pending_reply_snapshot(&second)[0].peer, "parent");
}

#[test]
fn later_sends_replace_earlier_questions_without_reusing_old_answers() {
    let (_dir, mut hub, first, second) = fixture();
    hub.bus_send(&first, "parent", "first question").unwrap();
    let old_answer = hub.bus_send("parent", &first, "old answer").unwrap();
    let answered = hub.wait_snapshot("parent", None, old_answer.seq).unwrap();
    assert_eq!(answered.status, InboxWaitStatus::NothingPending);
    hub.bus_send(&first, "parent", "new question").unwrap();
    let newest = hub.bus_send(&first, "parent", "clarification").unwrap();
    let pending = hub.wait_snapshot("parent", None, old_answer.seq).unwrap();
    assert_eq!(pending.pending_replies.len(), 1);
    assert_eq!(pending.pending_replies[0].sent_cursor, newest.seq);
    assert_eq!(pending.status, InboxWaitStatus::Pending);

    let unsolicited = hub.bus_send("parent", &second, "new information").unwrap();
    let news = hub.wait_snapshot("parent", None, old_answer.seq).unwrap();
    assert_eq!(news.status, InboxWaitStatus::Messages);
    assert_eq!(news.messages[0].seq, unsolicited.seq);
    assert_eq!(news.pending_replies[0].peer, first);
    let still_waiting = hub.wait_snapshot("parent", None, news.next_cursor).unwrap();
    assert_eq!(still_waiting.status, InboxWaitStatus::Pending);
}

#[test]
fn automatic_wait_retains_a_fast_reply_received_between_questions() {
    let (_dir, mut hub, first, second) = fixture();
    hub.bus_send(&first, "parent", "first question").unwrap();
    let answer = hub.bus_send("parent", &first, "fast answer").unwrap();
    hub.bus_send(&second, "parent", "second question").unwrap();
    let result = hub.wait_snapshot("parent", None, 0).unwrap();
    assert_eq!(result.status, InboxWaitStatus::Messages);
    assert_eq!(result.messages[0].seq, answer.seq);
    assert_eq!(result.pending_replies.len(), 1);
    assert_eq!(result.pending_replies[0].peer, second);
    assert_eq!(
        hub.wait_snapshot("parent", None, result.next_cursor)
            .unwrap()
            .status,
        InboxWaitStatus::Pending
    );
}

#[test]
fn failed_delivery_neither_creates_nor_answers_a_pending_send() {
    let (_dir, mut hub, first, second) = fixture();
    let question = hub.bus_send(&second, &first, "question").unwrap();
    let first_path = hub.get(&first).unwrap().steer_path;
    std::fs::create_dir(&first_path).unwrap();
    assert!(
        hub.bus_send(&first, &second, "undeliverable answer")
            .is_err()
    );
    let pending = hub.wait_snapshot(&first, None, question.seq).unwrap();
    assert_eq!(pending.status, InboxWaitStatus::Pending);
    assert_eq!(pending.pending_replies[0].peer, second);
    assert!(hub.pending_reply_snapshot(&second).is_empty());
    assert!(
        hub.bus_send(&first, "parent", "undeliverable question")
            .is_err()
    );
    assert_eq!(
        hub.wait_snapshot("parent", None, 0).unwrap().status,
        InboxWaitStatus::NothingPending
    );
}

#[test]
fn reaped_peers_stop_automatic_reply_waits_before_result_writeback() {
    let (_dir, mut hub, first, _) = fixture();
    let question = hub.bus_send(&first, "parent", "question").unwrap();
    hub.mark_process_reaped(&first);
    let result = hub.wait_snapshot("parent", None, question.seq).unwrap();
    assert_eq!(result.status, InboxWaitStatus::PeerStopped);
    assert_eq!(
        result.pending_replies[0].peer_status,
        Some(ChildStatus::Running)
    );
    assert!(!result.pending_replies[0].can_reply);
}

#[test]
fn bounded_tracking_reports_eviction_without_rejecting_accepted_messages() {
    let (_dir, mut hub, first, second) = fixture();
    let evicted = hub.bus_send(&first, "parent", "old question").unwrap();
    let unrelated = hub.bus_send("parent", &second, "unrelated news").unwrap();
    let seen = hub.wait_snapshot("parent", None, 0).unwrap().next_cursor;
    assert_eq!(seen, unrelated.seq);
    assert!(seen > evicted.seq);
    for index in 0..MAX_TRACKED_PEERS {
        let peer = hub.register(&format!("bounded-{index}"), "task").unwrap();
        hub.mark_running(&peer.id, 23);
        hub.bus_send(&peer.id, "parent", "question").unwrap();
    }
    let lost = hub.wait_snapshot("parent", None, seen).unwrap();
    assert_eq!(lost.status, InboxWaitStatus::HistoryLost);
    assert_eq!(lost.pending_replies.len(), MAX_TRACKED_PEERS);
    assert_eq!(lost.dropped_tracking_through_cursor, hub.bus_seq);
    assert_eq!(lost.next_cursor, hub.bus_seq);
    assert_eq!(hub.inbox(&first)[0].seq, evicted.seq);
    assert_eq!(
        hub.wait_snapshot("parent", None, lost.next_cursor)
            .unwrap()
            .status,
        InboxWaitStatus::Pending
    );
    // Named reads remain independent of automatic tracking retention.
    let named = hub.wait_snapshot("parent", Some(&first), seen).unwrap();
    assert_eq!(named.dropped_tracking_through_cursor, 0);
    assert_eq!(named.status, InboxWaitStatus::Pending);
}

#[test]
fn tracking_byte_limit_cannot_overflow_an_ipc_wait_reply() {
    let (_dir, mut hub, first, _) = fixture();
    hub.bus_send(&first, "parent", "old question").unwrap();
    let oversized = hub
        .register(&"x".repeat(MAX_TRACKED_PEER_BYTES + 1), "task")
        .unwrap();
    // Exercise the accepted-delivery recorder with an unusually long native
    // identity independently of the host filesystem's filename length limit.
    hub.record_reply_delivery(&BusMessage {
        seq: 2,
        from: "parent".to_string(),
        to: oversized.id,
        body: String::new(),
        sent_ms: 0,
    });
    let lost = hub.wait_snapshot("parent", None, 0).unwrap();
    assert_eq!(lost.status, InboxWaitStatus::HistoryLost);
    assert!(lost.pending_replies.is_empty());
    assert_eq!(lost.dropped_tracking_through_cursor, 2);
    assert!(serde_json::to_vec(&lost).unwrap().len() < 1024);
}
