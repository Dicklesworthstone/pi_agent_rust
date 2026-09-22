use super::*;
use crate::agent::AbortSignal;
use crate::error::Error;
use crate::model::{ContentBlock, ImageContent, UserContent};
use crate::session_control::tests::live;
use crate::session_control::{InputKind, TurnGuard};
use asupersync::{Budget, Cx};
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn cancel(owner: &Cx) {
    owner.cancel_with(asupersync::types::CancelKind::User, Some("test owner stopped"));
}

fn never_started(
    guard: TurnGuard,
    polls: Arc<AtomicUsize>,
) -> impl Future<Output = Result<AssistantMessage>> {
    async move {
        let _guard = guard;
        polls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[test]
fn construction_owner_survives_poll_migration_and_parent_is_restored() {
    let owner = Cx::for_request_with_budget(Budget::new().with_poll_quota(17));
    let (control, guard, _) = live();
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&polls);
    let mut turn = {
        let restricted = owner.restrict::<asupersync::cx::cap::None>();
        let _current = restricted.set_current_restricted();
        let budget = Cx::current().unwrap().budget();
        let capabilities = Cx::current().unwrap().capabilities();
        OwnedTurn::new(
            async move {
                let _guard = guard;
                poll_fn(move |_| {
                    let current = Cx::current().expect("owner installed during native poll");
                    assert_eq!(current.capabilities(), capabilities);
                    assert_eq!(current.budget(), budget);
                    if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                        Poll::Pending
                    } else {
                        Poll::Ready(Ok(AssistantMessage::default()))
                    }
                })
                .await
            },
            control.clone(),
        )
    };
    let parent = Cx::for_request_with_budget(Budget::new().with_poll_quota(43));
    let parent_caps = parent.capabilities();
    let _parent = parent.clone().set_current_restricted();
    let waker = Waker::noop();
    assert!(Pin::new(&mut turn).poll(&mut Context::from_waker(waker)).is_pending());
    assert_eq!(Cx::current().unwrap().capabilities(), parent_caps);
    assert_eq!(Cx::current().unwrap().budget(), parent.budget());

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let other = Cx::for_request_with_budget(Budget::new().with_poll_quota(59));
            let _other = other.clone().set_current_restricted();
            assert!(matches!(
                Pin::new(&mut turn).poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(_))
            ));
            assert_eq!(Cx::current().unwrap().budget(), other.budget());
            assert!(!other.is_cancel_requested());
        }).join().unwrap();
    });
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert!(control.snapshot().finished);
    assert!(!owner.is_cancel_requested());
    assert!(!parent.is_cancel_requested());
}

#[test]
fn cancelled_owner_prevents_first_native_poll_and_preserves_attachments() {
    let owner = Cx::for_request();
    let (control, guard, _) = live();
    let content = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
        data: "aGVsbG8=".to_string(),
        mime_type: "image/png".to_string(),
    })]);
    control.follow_up_with_content(&content, "inspect later").unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut turn = {
        let _current = owner.clone().set_current_restricted();
        OwnedTurn::new(never_started(guard, Arc::clone(&polls)), control.clone())
    };
    cancel(&owner);
    let other = Cx::for_request();
    let _other = other.clone().set_current_restricted();
    let Poll::Ready(Err(error)) = Pin::new(&mut turn)
        .poll(&mut Context::from_waker(Waker::noop())) else {
        panic!("cancelled unused turn must return without dispatch");
    };
    assert!(error.to_string().contains("SESSION_CONTROL_CANCELLED"));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(control.snapshot().finished);
    assert_eq!(control.snapshot().handed_to_agent, 0);
    let pending = control.take_pending();
    assert_eq!(pending[0].kind, InputKind::FollowUp);
    assert_eq!(pending[0].text, "inspect later");
    assert_eq!(serde_json::to_value(&pending[0].content).unwrap(), serde_json::to_value(content).unwrap());
    assert!(!other.is_cancel_requested());
}

#[test]
fn explicit_abort_before_first_poll_does_not_enter_the_sdk() {
    let (control, guard, _) = live();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut turn = OwnedTurn::new(never_started(guard, Arc::clone(&polls)), control.clone());
    assert!(control.abort());
    assert!(matches!(
        Pin::new(&mut turn).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(control.snapshot().finished);
}

fn draining_native(
    guard: TurnGuard,
    signal: AbortSignal,
    polls: Arc<AtomicUsize>,
) -> impl Future<Output = Result<AssistantMessage>> {
    async move {
        let _guard = guard;
        let mut draining = false;
        poll_fn(move |task| {
            polls.fetch_add(1, Ordering::SeqCst);
            if !signal.is_aborted() {
                return Poll::Pending;
            }
            if !draining {
                draining = true;
                task.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "native persistence failure must survive cancellation",
            ).into()))
        }).await
    }
}

#[test]
fn owner_cancellation_wakes_a_parked_turn_and_drains_native_cleanup() {
    let owner = Cx::for_request();
    let (control, guard, signal) = live();
    control.steer("retain this unclaimed input").unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut turn = {
        let _current = owner.clone().set_current_restricted();
        OwnedTurn::new(draining_native(guard, signal, Arc::clone(&polls)), control.clone())
    };
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut task = Context::from_waker(&waker);
    assert!(Pin::new(&mut turn).poll(&mut task).is_pending());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    // The native future deliberately registers no I/O or timer waker. Only
    // the explicit owner's cancellation subscription can wake this task.
    cancel(&owner);
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(Pin::new(&mut turn).poll(&mut task).is_pending());
    assert!(!control.snapshot().accepting_input);
    assert!(!control.snapshot().finished, "native cleanup is still pending");
    assert_eq!(control.snapshot().pending_steering, 1);
    let Poll::Ready(Err(Error::Io(error))) = Pin::new(&mut turn).poll(&mut task) else {
        panic!("retain the native typed error, not a synthetic cancellation success");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert!(control.snapshot().finished);
    assert_eq!(control.take_pending()[0].text, "retain this unclaimed input");
}

#[test]
fn completed_turn_retires_cancellation_subscription() {
    let owner = Cx::for_request();
    let (control, guard, _) = live();
    let mut turn = {
        let _current = owner.clone().set_current_restricted();
        OwnedTurn::new(async move {
            let _guard = guard;
            Ok(AssistantMessage::default())
        }, control.clone())
    };
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&wakes));
    assert!(Pin::new(&mut turn).poll(&mut Context::from_waker(&waker)).is_ready());
    let before = wakes.0.load(Ordering::SeqCst);
    cancel(&owner);
    assert_eq!(wakes.0.load(Ordering::SeqCst), before);
    assert!(control.snapshot().finished);
}

struct DropOwner {
    owner_budget: Budget,
    count: Arc<AtomicUsize>,
}

impl Drop for DropOwner {
    fn drop(&mut self) {
        assert_eq!(Cx::current().expect("owner installed for drop").budget(), self.owner_budget);
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn unpolled_drop_retires_captured_resources_under_their_owner() {
    let owner = Cx::for_request_with_budget(Budget::new().with_poll_quota(23));
    let count = Arc::new(AtomicUsize::new(0));
    let (control, guard, _) = live();
    let turn = {
        let _current = owner.clone().set_current_restricted();
        let witness = DropOwner { owner_budget: owner.budget(), count: Arc::clone(&count) };
        OwnedTurn::new(async move {
            let _guard = guard;
            let _witness = witness;
            std::future::pending::<Result<AssistantMessage>>().await
        }, control.clone())
    };
    let other = Cx::for_request_with_budget(Budget::new().with_poll_quota(31));
    let _other = other.clone().set_current_restricted();
    drop(turn);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(control.snapshot().finished);
    assert_eq!(Cx::current().unwrap().budget(), other.budget());
}

#[test]
fn outside_context_construction_binds_once_on_first_poll() {
    // A new OS thread has no ambient runtime context, even when other tests do.
    std::thread::spawn(|| {
        assert!(Cx::current().is_none());
        let (control, guard, _) = live();
        let owner = Cx::for_request_with_budget(Budget::new().with_poll_quota(37));
        let expected = owner.budget();
        let mut first = true;
        let mut turn = OwnedTurn::new(async move {
            let _guard = guard;
            poll_fn(move |_| {
                assert_eq!(Cx::current().unwrap().budget(), expected);
                if first {
                    first = false;
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(AssistantMessage::default()))
                }
            }).await
        }, control.clone());
        assert!(turn.owner.is_none());
        {
            let _current = owner.clone().set_current_restricted();
            assert!(Pin::new(&mut turn).poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        assert!(Cx::current().is_none());
        let other = Cx::for_request();
        let _other = other.clone().set_current_restricted();
        assert!(Pin::new(&mut turn).poll(&mut Context::from_waker(Waker::noop())).is_ready());
        assert_eq!(Cx::current().unwrap().budget(), other.budget());
        assert!(control.snapshot().finished);
    }).join().unwrap();
}

#[path = "agent_tests.rs"]
mod agent_tests;
