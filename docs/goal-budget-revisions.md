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
active-interval accounting, native failed-save recovery and reopening. These
are authored Rust tests, not a claim of a passing quality gate. Run the
repository's authoritative `dsr quality --tool pi_agent_rust` workflow.
