//! Real Git writeback under a contended serialization lock and revoked authority.

use super::*;
use crate::agent_cx::AgentCx;
use std::fs;
use std::sync::mpsc;
use std::thread;

/// Hold the actual production lock on another thread. The watchdog makes a
/// regression to a blocking async path fail instead of hanging the test runner.
pub(crate) struct HeldApply {
    release: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<bool>>,
}

impl HeldApply {
    pub(crate) fn new() -> Self {
        let (ready, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _guard = parent_apply_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if ready.send(()).is_err() {
                return false;
            }
            released.recv_timeout(Duration::from_secs(30)).is_ok()
        });
        let held = Self {
            release: Some(release),
            worker: Some(worker),
        };
        started
            .recv_timeout(Duration::from_secs(60))
            .expect("another writer owns the apply lock");
        held
    }

    pub(crate) fn release(mut self) {
        let sent = self.release.take().unwrap().send(()).is_ok();
        let received = self.worker.take().unwrap().join().unwrap();
        assert!(
            sent && received,
            "writeback blocked until the watchdog released the apply lock"
        );
    }
}

impl Drop for HeldApply {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn fixture() -> (tempfile::TempDir, IsoHandle, String) {
    let repo = tempfile::tempdir().unwrap();
    git_ok(repo.path(), &["init", "-b", "main"]).unwrap();
    git_ok(repo.path(), &["config", "user.name", "Writeback Fixture"]).unwrap();
    git_ok(repo.path(), &["config", "user.email", "writeback@localhost"]).unwrap();
    git_ok(repo.path(), &["config", "commit.gpgSign", "false"]).unwrap();
    fs::write(repo.path().join("tracked.txt"), "original\n").unwrap();
    git_ok(repo.path(), &["add", "tracked.txt"]).unwrap();
    git_ok(repo.path(), &["commit", "-m", "initial"]).unwrap();
    let handle = isolate(repo.path(), "queued-writeback").unwrap();
    fs::write(handle.path.join("tracked.txt"), "child edit\n").unwrap();
    let (patch, _) = collect_diff(&handle).unwrap();
    (repo, handle, patch)
}

#[test]
fn owned_writeback_yields_and_cancels_without_waiting_for_the_other_writer() {
    let (repo, handle, patch) = fixture();
    let held = HeldApply::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_request();
        let mut applying = Box::pin(apply_to_parent_owned(&handle, &patch, &owner, || {
            owner.checkpoint().is_ok()
        }));
        assert!(futures::poll!(&mut applying).is_pending());
        owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel writeback"));
        assert!(!applying.await.unwrap());
    });
    assert_eq!(
        fs::read(repo.path().join("tracked.txt")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(handle.path.join("tracked.txt")).unwrap(),
        b"child edit\n"
    );
    held.release();

    // A later authorized retry can still use the exact preserved patch.
    let owner = AgentCx::for_request();
    assert!(
        runtime
            .block_on(apply_to_parent_owned(&handle, &patch, &owner, || true))
            .unwrap()
    );
    assert_eq!(
        fs::read(repo.path().join("tracked.txt")).unwrap(),
        b"child edit\n"
    );
    drop_worktree(&handle).unwrap();
}

#[test]
fn authority_is_rechecked_after_successful_git_preflight_before_mutation() {
    let (repo, handle, patch) = fixture();
    let apply_guard = parent_apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut checkpoints = 0;
    assert!(
        !apply_authorized(&handle, &patch, &mut || {
            checkpoints += 1;
            checkpoints == 1
        })
        .unwrap()
    );
    assert_eq!(checkpoints, 2);
    assert_eq!(
        fs::read(repo.path().join("tracked.txt")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(handle.path.join("tracked.txt")).unwrap(),
        b"child edit\n"
    );

    // A rejected preflight must remain a real conflict, not a fabricated
    // cancellation. Its failure precedes the final dispatch checkpoint.
    fs::write(repo.path().join("tracked.txt"), "conflicting parent edit\n").unwrap();
    checkpoints = 0;
    let error = apply_authorized(&handle, &patch, &mut || {
        checkpoints += 1;
        checkpoints == 1
    })
    .unwrap_err();
    assert!(error.to_string().contains("PI_ISO_CONFLICT"));
    assert_eq!(checkpoints, 1);
    drop(apply_guard);
    drop_worktree(&handle).unwrap();
}

#[test]
fn committed_writeback_never_claims_a_later_authority_check_rolled_it_back() {
    let (repo, handle, patch) = fixture();
    let apply_guard = parent_apply_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        apply_authorized(&handle, &patch, &mut || {
            fs::read(repo.path().join("tracked.txt")).unwrap() == b"original\n"
        })
        .unwrap(),
        "authority that expires after the actual apply cannot retract its outcome"
    );
    assert_eq!(
        fs::read(repo.path().join("tracked.txt")).unwrap(),
        b"child edit\n"
    );
    drop(apply_guard);
    drop_worktree(&handle).unwrap();
}
