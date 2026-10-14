use super::*;
use std::future::{pending, ready};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::task::Wake;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
}

fn notification(token: &str, progress: Value) -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/progress",
        "params":{"progressToken":token,"progress":progress}})
}

fn subscribed(registry: &Arc<ProgressRegistry>, generation: u64) -> ProgressSubscription {
    let dispatch = Arc::new(RequestDispatch::new());
    assert!(dispatch.claim());
    registry.subscribe(dispatch, generation).unwrap()
}

fn poll(subscription: &mut ProgressSubscription) -> Poll<Option<ToolUpdate>> {
    subscription.poll_update(&mut Context::from_waker(Waker::noop()))
}

fn progress(subscription: &mut ProgressSubscription) -> Value {
    let Poll::Ready(Some(update)) = poll(subscription) else { panic!("expected progress") };
    update.details.unwrap()["mcpProgress"].clone()
}

#[test]
fn queued_observer_registers_a_waker_but_accepts_no_undispatched_progress() {
    let registry = Arc::new(ProgressRegistry::default());
    let dispatch = Arc::new(RequestDispatch::new());
    let mut subscription = registry.subscribe(Arc::clone(&dispatch), 0).unwrap();
    let message = notification(subscription.token(), json!(1));
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&wakes));
    assert!(subscription.poll_update(&mut Context::from_waker(&waker)).is_pending());
    registry.publish(&message, 0);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    assert!(dispatch.claim());
    registry.publish(&message, 0);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert_eq!(progress(&mut subscription)["progress"], 1);
}

#[test]
fn tokens_connections_and_http_generations_are_isolated() {
    let registry = Arc::new(ProgressRegistry::default());
    let other = Arc::new(ProgressRegistry::default());
    let mut first = subscribed(&registry, 7);
    let mut second = subscribed(&registry, 8);
    let mut foreign = subscribed(&other, 7);
    assert_ne!(first.token(), second.token());
    assert_ne!(first.token(), foreign.token());
    let message = notification(first.token(), json!(3.5));
    registry.publish(&message, 8);
    other.publish(&message, 7);
    assert!(poll(&mut first).is_pending());
    assert!(poll(&mut second).is_pending());
    assert!(poll(&mut foreign).is_pending());
    registry.publish(&message, 7);
    assert_eq!(progress(&mut first)["progress"], 3.5);
    assert!(poll(&mut second).is_pending());
}

#[test]
fn malformed_and_nonincreasing_progress_does_not_poison_later_updates() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscription = subscribed(&registry, 0);
    let token = subscription.token().to_string();
    registry.publish(&notification(&token, json!(1.5)), 0);
    assert_eq!(progress(&mut subscription)["progress"], 1.5);
    for value in [Value::Null, json!("2"), json!(-1), json!(1.5), json!(1.25)] {
        registry.publish(&notification(&token, value), 0);
        assert!(poll(&mut subscription).is_pending());
    }
    for field in ["message", "total"] {
        let mut malformed = notification(&token, json!(2));
        malformed["params"][field] = json!({"not":"the required scalar"});
        registry.publish(&malformed, 0);
        assert!(poll(&mut subscription).is_pending());
    }
    for field in ["id", "result", "error"] {
        let mut not_a_notification = notification(&token, json!(2));
        not_a_notification[field] = Value::Null;
        registry.publish(&not_a_notification, 0);
        assert!(poll(&mut subscription).is_pending());
    }
    let mut wrong_token_type = notification(&token, json!(2));
    wrong_token_type["params"]["progressToken"] = json!(1);
    registry.publish(&wrong_token_type, 0);
    assert!(poll(&mut subscription).is_pending());
    registry.publish(&notification(&token, json!(2)), 0);
    assert_eq!(progress(&mut subscription)["progress"], 2);
}

#[test]
fn large_integer_progress_does_not_round_through_f64() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscription = subscribed(&registry, 0);
    for value in [9_007_199_254_740_992_u64, 9_007_199_254_740_993, u64::MAX] {
        registry.publish(&notification(subscription.token(), json!(value)), 0);
        assert_eq!(progress(&mut subscription)["progress"], value);
    }
}

#[test]
fn producer_flood_coalesces_and_delivery_has_a_hard_bound() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscription = subscribed(&registry, 0);
    for value in 0..10_000 {
        registry.publish(&notification(subscription.token(), json!(value)), 0);
    }
    assert_eq!(progress(&mut subscription)["progress"], 9999);
    assert!(poll(&mut subscription).is_pending());
    for offset in 1..MAX_UPDATES {
        registry.publish(&notification(subscription.token(), json!(10_000 + offset)), 0);
        assert_eq!(progress(&mut subscription)["deliveryLimited"], offset == MAX_UPDATES - 1);
    }
    registry.publish(&notification(subscription.token(), json!(20_000)), 0);
    assert!(matches!(poll(&mut subscription), Poll::Ready(None)));
    assert!(lock(&subscription.slot.state).pending.is_none());
}

#[test]
fn message_rendering_is_bounded_escaped_and_not_dumped_in_debug() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscription = subscribed(&registry, 0);
    let mut value = notification(subscription.token(), json!(0));
    value["params"]["total"] = json!(10.5);
    value["params"]["message"] = json!(format!("secret\x1b[2J\n\u{202e}{}", "Δ".repeat(10_000)));
    registry.publish(&value, 0);
    let update = progress(&mut subscription);
    let message = update["message"].as_str().unwrap();
    assert!(message.len() <= MAX_MESSAGE_BYTES);
    assert!(message.is_ascii());
    assert!(!message.chars().any(char::is_control));
    assert!(message.contains("\\u{1b}"));
    assert!(message.contains("\\u{202e}"));
    assert_eq!(update["messageTruncated"], true);
    assert_eq!(update["total"], 10.5);
    assert!(!format!("{registry:?} {subscription:?}").contains("secret"));
    assert!(!format!("{subscription:?}").contains(subscription.token()));
}

#[test]
fn dropping_removes_waiters_and_late_tokens_cannot_reach_replacement_work() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscription = subscribed(&registry, 0);
    let old_token = subscription.token().to_string();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&wakes));
    assert!(subscription.poll_update(&mut Context::from_waker(&waker)).is_pending());
    assert_eq!(Arc::strong_count(&wakes), 3);
    drop(subscription);
    assert!(lock(&registry.slots).is_empty());
    assert_eq!(Arc::strong_count(&wakes), 2);
    let mut replacement = subscribed(&registry, 0);
    registry.publish(&notification(&old_token, json!(100)), 0);
    assert!(poll(&mut replacement).is_pending());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
}

#[test]
fn capacity_and_counter_exhaustion_never_evict_existing_requests() {
    let registry = Arc::new(ProgressRegistry::default());
    let mut subscriptions: Vec<_> = (0..MAX_SUBSCRIPTIONS).map(|_| subscribed(&registry, 0)).collect();
    let dispatch = Arc::new(RequestDispatch::new());
    assert!(registry.subscribe(Arc::clone(&dispatch), 0).is_none());
    assert_eq!(lock(&registry.slots).len(), MAX_SUBSCRIPTIONS);
    subscriptions.pop();
    assert!(registry.subscribe(Arc::clone(&dispatch), 0).is_some());
    registry.next_id.store(u64::MAX, Ordering::SeqCst);
    assert!(registry.subscribe(dispatch, 0).is_none());
    registry.publish(&notification(subscriptions[0].token(), json!(1)), 0);
    assert_eq!(progress(&mut subscriptions[0])["progress"], 1);
}

#[test]
fn a_ready_result_wins_over_queued_progress_and_unregisters() {
    let registry = Arc::new(ProgressRegistry::default());
    let subscription = subscribed(&registry, 0);
    registry.publish(&notification(subscription.token(), json!(100)), 0);
    let callback = |_update| panic!("a result must not be replaced by a progress callback");
    let mut future = Box::pin(observe(ready(Err::<(), _>("real server error")), Some(subscription), Some(&callback), || true));
    let result = future.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    assert_eq!(result, Poll::Ready(Err("real server error")));
    assert!(lock(&registry.slots).is_empty());
}

#[test]
fn callback_can_reenter_registry_and_a_dropped_observer_cleans_up() {
    let registry = Arc::new(ProgressRegistry::default());
    let subscription = subscribed(&registry, 0);
    registry.publish(&notification(subscription.token(), json!(10)), 0);
    let called = AtomicUsize::new(0);
    let callback = |_update| {
        drop(subscribed(&registry, 0));
        called.fetch_add(1, Ordering::SeqCst);
    };
    let mut future = Box::pin(observe(pending::<()>(), Some(subscription), Some(&callback), || true));
    assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert_eq!(called.load(Ordering::SeqCst), 1);
    drop(future);
    assert!(lock(&registry.slots).is_empty());
}

#[test]
fn revoked_owner_or_generation_suppresses_callbacks_without_inventing_a_result() {
    let registry = Arc::new(ProgressRegistry::default());
    let subscription = subscribed(&registry, 0);
    registry.publish(&notification(subscription.token(), json!(10)), 0);
    let current = AtomicBool::new(false);
    let callback = |_update| panic!("stale ownership must suppress progress");
    let mut future = Box::pin(observe(pending::<()>(), Some(subscription), Some(&callback), || current.load(Ordering::SeqCst)));
    assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert!(lock(&registry.slots).is_empty());
}

#[test]
fn dropping_an_unpolled_observer_never_polls_the_request() {
    let registry = Arc::new(ProgressRegistry::default());
    let subscription = subscribed(&registry, 0);
    let callback = |_update| panic!("unpolled callback");
    let operation = poll_fn(|_| -> Poll<()> { panic!("unpolled request") });
    drop(observe(operation, Some(subscription), Some(&callback), || true));
    assert!(lock(&registry.slots).is_empty());
}
