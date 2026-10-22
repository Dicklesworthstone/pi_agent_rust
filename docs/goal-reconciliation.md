# Reconcile an interrupted goal without replay

A saved goal can contain an intent whose native attempt never settled: for
example, the process ended after a tool changed a file but before the final
checkpoint was saved. `interrupted: true` is uncertainty about that attempt,
not permission to repeat it and not evidence that its work failed.

After inspecting session history, external side effects and returned inputs,
a host can now resolve that uncertainty explicitly. It does not need to resume
provider work or increase an exhausted budget merely to record an outcome.

## Native SDK

```rust,no_run
use pi::agent_cx::AgentCx;
use pi::goal::GoalReconciliation;
use pi::sdk::{AgentSessionHandle, Result};

async fn reconcile(handle: &mut AgentSessionHandle, owner: &AgentCx) -> Result<()> {
    let reviewed = handle.current_goal()?.expect("review an existing goal first");
    let changed = handle.reconcile_goal(
        owner,
        &reviewed,
        GoalReconciliation::Complete,
        "I inspected the committed output and actual verification results after restart.",
    ).await?;
    // Inspect changed.persistence; Unconfirmed does not undo the live decision.
    // Repair storage and call flush_goal rather than repeating reconciliation.
    let _ = changed;
    Ok(())
}
```

Choose one of three explicit outcomes:

* `Pause` acknowledges that the interrupted attempt was inspected, but leaves
  the objective for review. A spent allowance still reports budget-limited.
  Resuming or revising a budget remains a separate user decision.
* `Complete` records the host's completion attestation, even if the attempt
  consumed the final prompt or token allowance.
* `Drop` abandons the objective without claiming successful completion.

All outcomes preserve the objective, goal identity, consumed counters and total
allowances. One new checkpoint records the literal evidence; the old unfinished
intent stays in the journal. Evidence must be nonblank and at most 16 KiB of
UTF-8. It is not parsed as a command or independently verified by Pi.

A fresh snapshot of the exact store, selected branch, leaf and current goal is
required. New usage or history, a different store, a corrupt checkpoint,
cancellation, contention, an already-settled intent or a terminal goal refuses
the decision. Repeated reconciliation is not a no-op: review the new state.

## Ordinary pause is not reconciliation

SDK `pause_goal` and ordinary RPC pause now reject unfinished intents with
`GOAL_INTERRUPTED`, matching the terminal's existing refusal. Previously, plain
pause could clear `inFlight`, after which an ordinary completion could succeed
without an explicit reconciliation decision. The shared transition now closes
that path. Pausing a live runner with `GoalControl::pause` is unchanged: its
native cancellation, drain and settlement remain authoritative.

Explicit resume and drop retain their existing contracts. Reconciliation adds
an outcome-recording path that does not authorize more execution. It does not
repair history, replay tools, restore/replay unsent input, clear an SDK
provider-admission quarantine, or prove abandoned asynchronous cleanup finished.
A host must resolve those separate concerns before further work.

## Persistence and validation

`GoalChange::persistence` is still `Saved`, `MemoryOnly` or `Unconfirmed`.
Unconfirmed persistence leaves the decision in memory. Retrying `flush_goal`
saves that state without appending another decision; reopening a successfully
saved file preserves both the original intent and the reconciled checkpoint.
The existing session storage and durability guarantees are unchanged.

Nine native journal/SDK regressions cover the ordinary-pause bypass, all three
outcomes with exhausted and unspent budgets, explicit resume, repeated and
stale decisions, evidence bounds, changed usage/branches/stores, cancellation,
contention, corrupt checkpoints, native save/reopen and failed-save recovery.
They use actual AgentSession and Session APIs with a provider that records and
refuses calls. They do not execute a live model or prove cross-platform behavior.

These Rust tests are authored, not executed on the implementation host. The
required `dsr quality --tool pi_agent_rust` could not start because `dsr` is
absent (exit 127). Compilation, Rust tests, formatting and Clippy remain
unverified. The terminal keyboard-control patch is not part of this change.
