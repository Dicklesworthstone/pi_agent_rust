# Revise a goal budget without discarding its work

An SDK host can explicitly change the **total** prompt and token allowances of
an existing goal with `AgentSessionHandle::set_goal_budget`. This lets a host
retain an exhausted objective, recorded usage and journal history instead of
dropping it and creating replacement work with fresh counters. The method is
available with or without the `ftui` feature.

```rust
use pi::agent_cx::AgentCx;
use pi::goal::GoalBudget;
use pi::sdk::{AgentSessionHandle, Error, Result};

async fn authorize_more_work(handle: &mut AgentSessionHandle) -> Result<()> {
    let owner = AgentCx::for_current_or_request();
    let reviewed = handle.current_goal()?.ok_or_else(|| Error::validation("No goal"))?;
    // The application should present this snapshot and proposed totals for
    // review before calling the setter, not infer authorization from model text.
    let changed = handle.set_goal_budget(
        &owner,
        &reviewed,
        GoalBudget { max_prompts: 24, token_budget: 250_000 },
        "Reviewed the remaining migration and authorized these new totals",
    ).await?;
    if !changed.persistence.allows_continuation() {
        return Err(Error::session("Budget changed in memory; inspect and flush before resuming"));
    }
    // No provider request has started. Inspect changed.goal; resume_goal and
    // a bounded run_goal invocation are separate, explicit operations.
    Ok(())
}
```

Both values are totals, not increments. Revising 100,000 tokens to 150,000 after
spending 80,000 leaves 70,000, not 150,000. Recorded tokens and prompts never
reset or decrease. The same objective and goal ID remain attached to the same
branch. The previous checkpoint retains the prior limits, and the new one
records the authored reason. There is no automatic budget increase.

## Default terminal

Run `/goal status` and review the displayed objective and usage. Copy its actual
`g1-...` receipt into the command; both proposed totals and a reason are required:

```text
/goal budget <review> --tokens 150000 --prompts 24 -- Reviewed the remaining migration
```

The result shows the new totals, unchanged usage, recorded reason and fresh
receipt. Use that new receipt for `/goal resume` after inspecting the result.
No goal execution starts from either command. Missing or repeated flags,
unlimited settings, stale receipts and empty reasons fail without appending a
goal decision. Text after `--` is literal, not a shell command or file reference.
Existing `/goal create` defaults are unchanged; budget revision never supplies
an omitted limit from those defaults. `/goal save <new-review>` retries an
unconfirmed native save without replaying the budget decision.

## RPC hosts: pause and revise atomically

First send `{"type":"goal","op":"get"}`. Its response data includes the goal's
totals, usage, note and an additive `revision` field. After reviewing that state,
copy `data.revision` into a pause request with a `budget` object:

```json
{
  "type": "goal",
  "op": "pause",
  "revision": "<actual gr1-... receipt from goal get>",
  "budget": { "token_budget": 150000, "max_prompts": 24 },
  "reason": "Reviewed the remaining migration"
}
```

This is one journal transition, not a pause followed by an unguarded update.
It uses the existing idle-only RPC pause dispatch and terminal-input recovery
preflight. The nested budget must contain exactly both non-null integer totals.
The result carries a new `revision`, the retained usage, `note`, paused or
budget-limited state, and the existing persistence acknowledgement. No separate
`op: "budget"`, model-facing operation or new command-dispatch lane is added.
A budget attached to create/resume/drop is rejected rather than ignored.
Plain RPC pause requests retain their existing behavior.

The RPC receipt binds session ID, branch leaf, the full goal record including
recorded usage, and replayed plan state. It identifies logical state, not tool
authority, authentication or proof of review. It does not bind an in-memory
store incarnation, live settings or a save acknowledgement; unchanged saved
state can retain its receipt after reopening. It is separate from terminal
receipts and the SDK's exact snapshot. Input recovery that advances the journal
also makes a pending budget receipt stale. Read again rather than retrying it.

An unconfirmed RPC save still leaves the new totals live and paused. Do not
replay the old pause-and-revise request. Inspect the returned state and repair
native persistence; `flush_goal` is the SDK retry operation. No new RPC save
operation or automatic recovery execution is introduced here.

## Review, pause and interruption

The setter requires the SDK's exact `GoalSnapshot`. Another branch, input,
checkpoint, replacement goal or storage incarnation makes it stale. Validation
and append share the native journal lock. A stale, cancelled, contended or
invalid request appends nothing. A live runner exclusively borrows the handle;
stop and review that run before changing its allowance.

A revised settled goal is **paused**, even when the new limits permit more work.
Call `resume_goal` with a fresh reviewed snapshot and then explicitly run it.
Neither changing a budget nor resuming starts a provider request by itself.
The existing run deadline, tool permissions, approvals and verification rules
are unchanged. A new explicit run chooses its own bounded time allowance.

An unfinished intent remains `interrupted` after revision. Raising its limits
is not an acknowledgement that the prior work succeeded and cannot authorize
completion. Inspect history and side effects before explicitly resuming or
dropping that intent. Spent prompt attempts remain spent, including interrupted
ones. Completed and dropped goals cannot be reopened through budget revision.

## Limits and persistence

Prompt totals remain 1–1024 and cannot be less than the number already started.
Token totals must be positive; there is no `off` or unlimited setting. A token
total at or below recorded usage is allowed but leaves the goal budget-limited.
Saturated usage never wraps into new allowance. An unchanged pair of limits is
rejected rather than appending a redundant decision. Reasons must be nonblank
and at most 16 KiB. Goal token accounting remains a continuation guardrail,
not a hard billing cap for an already-running native prompt.

The returned `GoalChange` separates the live transition from persistence.
`Saved` acknowledges the native save, `MemoryOnly` identifies an ephemeral
session, and `Unconfirmed` means the new limits exist in memory but the save
was not confirmed. Retry `flush_goal`, not the stale budget decision. The
revised paused state and all spent counters survive a successful save/reopen.
This is not a new file format or a cross-process execution lease.

## Validation status

`src/goal/budget_tests.rs` covers total-budget arithmetic, interrupted intent,
terminal states, invalid inputs, revision rejection, branch/store isolation,
active-interval accounting, native failed-save recovery and reopening. It also
covers the default-terminal and RPC adapters with native sessions: required
fields, stale usage/session receipts, preserved interruption, literal reasons
and unconfirmed persistence. Existing driver routing and RPC dispatch are
reused, not replaced. These
are authored Rust tests, not a claim of a passing quality gate. Run the
repository's authoritative `dsr quality --tool pi_agent_rust` workflow.
