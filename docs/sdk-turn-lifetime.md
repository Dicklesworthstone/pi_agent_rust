# SDK turn lifetime and cancellation

An `AgentSessionHandle` prompt is a logical turn: provider attempts, tool work,
retries and the final session persistence step belong to that one future.
A provider `Done` event is not a durable acknowledgement of the whole turn.

## Cancel without abandoning completed work

Use an explicit abort signal and await the prompt's result, or use the existing
controlled-turn API. A control handle can be passed to another thread or kept by
the host UI while the turn borrows its session:

```rust,ignore
let turn = handle.prompt_controlled("Inspect the project".to_string(), on_event);
let control = turn.control();
// The host can call control.abort() while it continues polling this turn.
let outcome = turn.await;
```

For a deadline, use the native draining wrapper rather than racing a prompt
against a timer and dropping the losing prompt:

```rust,ignore
use std::time::Duration;
use pi::agent_cx::AgentCx;
use pi::session_control::TurnDeadline;

// Construct inside the SDK runtime; the deadline retains this owner's clock.
let deadline = TurnDeadline::after(
    &AgentCx::for_current_or_request(),
    Duration::from_secs(60),
)?;
let turn = handle
    .prompt_controlled("Inspect the project".to_string(), on_event)
    .with_deadline(deadline);
let outcome = turn.await;
```

Expiry requests cooperative abort and keeps polling the same native turn until
cleanup returns. Cleanup can outlive the deadline; this is not preemption of a
blocking system call. Inspect `TurnDeadlineError::completion()` for the actual
native result, including a possible session-persistence error. A cooperatively
aborted turn whose persistence succeeds does not acquire an abandonment fence.

## Dropped futures require explicit recovery

Once the SDK has admitted the turn, dropping its future skips any remaining
post-turn persistence. A tool may already have changed the filesystem even when
its result is absent from the saved transcript. The handle therefore refuses
subsequent text, image, native-content and continuation calls with a typed
session-persistence error containing `SDK_TURN_INTERRUPTED`. Existing, more
specific persistence failures remain a reason to refuse re-entry as well.

Abandonment seals the logical turn's retained callbacks. It does not emit a
synthetic `AgentEnd`, attempt a blocking save in `Drop`, undo tool effects, or
silently replay the original input. Start a new or resumed handle and reconcile
external effects before deciding which work to repeat. Reopening a saved session
alone does not prove that an interrupted tool made no changes.

Dropping an entirely unpolled prompt does not admit input or quarantine the
session. Input rejected during preflight and a signal already aborted at entry
also do not acquire this fence. These guarantees apply to the SDK recovery
entrypoints, not direct manipulation of a lower-level `Agent` or session store.
