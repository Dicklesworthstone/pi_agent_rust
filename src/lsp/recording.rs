//! One undo unit for an LSP invocation, including its admitted command batches.
//!
//! This owns no snapshots: the session's existing recorder supplies bounded
//! preimages, durable admission and restore policy. A captured invocation fences
//! late callbacks from both completed calls and a subsequent call's history.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::undo::FileMutationRecorder;

use super::tool_err;

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
pub(super) struct Recorder {
    recorder: Option<Arc<FileMutationRecorder>>,
    active: Mutex<Option<Arc<Invocation>>>,
}

impl Recorder {
    pub(super) fn new(recorder: Option<Arc<FileMutationRecorder>>) -> Self {
        Self {
            recorder,
            active: Mutex::new(None),
        }
    }

    pub(super) fn enter(&self, call_id: &str) -> Result<Scope<'_>> {
        let mut active = lock(&self.active);
        if active.is_some() {
            return Err(tool_err(
                "LSP_UNDO_BUSY",
                "LSP recording scope is already active",
            ));
        }
        let invocation = self.recorder.as_ref().map(|recorder| {
            Arc::new(Invocation {
                recorder: Arc::clone(recorder),
                call_id: call_id.to_owned(),
                state: Mutex::new(State::default()),
            })
        });
        *active = invocation.clone();
        drop(active);
        Ok(Scope {
            owner: self,
            invocation,
        })
    }

    pub(super) fn current(&self) -> Option<Arc<Invocation>> {
        lock(&self.active).clone()
    }
}

#[derive(Debug, Default)]
struct State {
    closed: bool,
    failed: bool,
    started: bool,
}

pub(in crate::lsp) struct Invocation {
    recorder: Arc<FileMutationRecorder>,
    call_id: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for Invocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The shared recorder contains unrelated file contents and history.
        formatter
            .debug_struct("Invocation")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Invocation {
    // Hold the fence through admission, commit and any synchronous rollback.
    // Scope finalization cannot capture a half-written callback's post-state.
    #[allow(clippy::significant_drop_tightening)]
    pub(in crate::lsp) fn apply<T>(
        &self,
        paths: &[PathBuf],
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut state = lock(&self.state);
        if state.closed || state.failed {
            return Err(tool_err(
                "LSP_UNDO_CLOSED",
                "LSP mutation belongs to a completed or failed tool call",
            ));
        }
        // Also fail closed if a native callback unwinds while committing.
        state.failed = true;
        if !paths.is_empty() {
            // An unsuccessful begin can itself retain a durable pending intent.
            state.started = true;
            if let Err(error) = self.recorder.begin_files(&self.call_id, "lsp", paths) {
                return Err(tool_err(
                    "LSP_UNDO_UNAVAILABLE",
                    format!("cannot record workspace edit before mutation: {error}"),
                ));
            }
        }
        let result = operation();
        state.failed = result.is_err();
        result
    }

    #[allow(clippy::significant_drop_tightening)]
    fn close(&self, succeeded: bool) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.closed = true;
        if state.started {
            if succeeded && !state.failed {
                self.recorder.commit(&self.call_id);
            } else {
                // Fully rolled-back attempts disappear; partial writes retain
                // the recorder's omission barrier instead of an unsafe undo.
                self.recorder.abort(&self.call_id);
            }
        }
    }
}

pub(super) struct Scope<'a> {
    owner: &'a Recorder,
    invocation: Option<Arc<Invocation>>,
}

impl Scope<'_> {
    pub(super) fn finish(mut self, succeeded: bool) {
        self.close(succeeded);
    }

    fn close(&mut self, succeeded: bool) {
        if let Some(invocation) = self.invocation.take() {
            invocation.close(succeeded);
            let mut active = lock(&self.owner.active);
            if active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &invocation))
            {
                *active = None;
            }
        }
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.close(false);
    }
}
