# Goal-driven SDK sessions

Pi's in-process SDK can now retain an objective across several native prompts,
stop for review, and resume with its remaining prompt/token budget. This is an
OMP-inspired **SDK workflow**. The execution driver does not install `/goal` in
either terminal interface or expose a model-facing `goal` tool. This page covers
in-process execution, not the separate RPC lifecycle adapter.
The types currently live in `pi::goal`, an unstable module under the library's
public API policy; the lifecycle methods are on `AgentSessionHandle`.

## Create an objective and drive it

Call `AgentSessionHandle::run_goal` on a borrowed handle, or convert it once with
`handle.into_goal_session()` for an owned convenience wrapper. Both use the same
driver and existing session rather than constructing a second agent. The handle
remains exclusively borrowed until the run is finished or dropped, then ordinary
session operations are available again. Provider selection, authentication, tools and their approval policies,
input hooks, retry/failover, compaction and native session persistence remain
owned by that session. Goal mode grants no additional tool permissions.

This helper accepts a host-owned result verifier. Call it inside the same
asupersync runtime used by the SDK:

```rust
use std::time::Duration;
use pi::agent_cx::AgentCx;
use pi::goal::{
    GoalBudget, GoalDecision, GoalRunReport, GoalSession, GoalSnapshot,
};
use pi::sdk::{AssistantMessage, Error, Result};

async fn execute_objective(
    session: &mut GoalSession,
    objective: &str,
    verify: impl FnMut(&GoalSnapshot, &AssistantMessage) -> GoalDecision + Send,
) -> Result<GoalRunReport> {
    let owner = AgentCx::for_current_or_request();
    let created = session.session_mut().create_goal(
        &owner,
        objective,
        GoalBudget { max_prompts: 8, token_budget: 50_000 },
    ).await?;
    if !created.persistence.allows_continuation() {
        return Err(Error::session(
            "Goal exists in memory, but its save is unconfirmed; inspect and flush it",
        ));
    }
    let run = session.run_goal(
        &owner,
        &created.goal,
        Duration::from_secs(15 * 60),
        |_control, _event| {},
        verify,
    )?;
    // Clone run.control() here for a UI or another thread before awaiting.
    Ok(run.await)
}
```

The verifier explicitly returns one of:

- `GoalDecision::Continue { instruction }`: journal the next bounded instruction
  and start another prompt only after persistence and budget checks.
- `GoalDecision::Pause { reason }`: retain the objective for explicit review.
- `GoalDecision::Complete { evidence }`: record the host's completion attestation.

Neither a normal assistant stop nor text such as “goal complete” supplies a host
verdict. Errors, refusal, truncated/aborted output, empty final answers and
unresolved tool calls do not reach this verifier. Evidence is an explanation
provided by the host, **not a proof independently verified by Pi**. The host
should inspect the actual deliverables and verification results. The callback
is synchronous; it cannot be preempted. The driver rechecks cancellation and
time after the callback and again before publishing its decision.

## Await real host verification

`run_goal_with_verifier` is available on both `AgentSessionHandle` and
`GoalSession`. Its host-supplied callback returns a future with output
`Result<GoalDecision>`, so it can await an external validator or other evidence
instead of blocking the executor or accepting an assistant's completion claim.
For a reviewed, active goal, an embedding can use:

```rust
use std::future::Future;
use std::time::Duration;
use pi::agent_cx::AgentCx;
use pi::goal::{GoalDecision, GoalRunReport, GoalSnapshot};
use pi::sdk::{AgentSessionHandle, AssistantMessage, Result};

async fn run_checked<F>(
    session: &mut AgentSessionHandle,
    expected: &GoalSnapshot,
    verify: impl FnMut(GoalSnapshot, AssistantMessage) -> F + Send,
) -> Result<GoalRunReport>
where
    F: Future<Output = Result<GoalDecision>> + Send,
{
    let owner = AgentCx::for_current_or_request();
    let run = session.run_goal_with_verifier(
        &owner,
        expected,
        Duration::from_secs(15 * 60),
        |_control, _event| {},
        verify,
    )?;
    // Publish run.control() to the host UI before awaiting as needed.
    Ok(run.await)
}
```

Each callback receives owned snapshots of the exact current goal and last
successful native response, not the whole history, mutable session, credentials,
or additional tool authority. The callback decides what constitutes acceptable
evidence. Pi does not independently certify that evidence. The synchronous
`run_goal` API remains available and uses the same verification/commit path.

Verification shares the absolute deadline constructed at run creation, including
time already spent on provider requests, retries and previous checks. Waiting
does not pause or restart that clock. `GoalControl::pause()` wakes even a silent
verifier with no native prompt running. Owner cancellation and timer expiry also
wake it without a polling task. A late success, including one returned by a poll
which itself paused the run or crossed the deadline, cannot authorize another
prompt or complete the goal. The final journal transition rechecks the exact
snapshot, pause state and deadline as well.

An ordinary verifier error stops with `GoalRunStop::Failed` and pauses the goal;
the error is reported separately from the preserved native `completion`. A later
goal-journal error can take precedence in `error`; inspect persistence and obtain
a fresh snapshot before acting. Invalid empty/oversized evidence fails closed.
Native errors, refusal, incomplete answers, unclaimed user inputs and native
run-time-cap messages do not even construct a verifier future. A native time cap
pauses instead of starting another prompt with a fresh cap.

The host must own cancellation-safe cleanup for its verifier's resources.
Pause, expiry or cancellation drops the pending verifier under its initiating
context and removes its timer/control registrations. This does not join detached
host tasks, forcibly terminate arbitrary processes, roll back side effects, or
preempt a blocking callback/poll. Native agent turns still use their existing
abort-and-drain path. A cancelled owner may be unable to persist the goal's pause;
the interrupted intent remains visible and is not permission to replay work.

Dropping during verification leaves the prior unfinished intent interrupted,
even if the native prompt already returned successfully. An unused unpolled run
constructs neither a verifier nor a provider request. Verification has no live
native input mailbox: `steer`/`follow_up` reject input there instead of silently
queueing it for another prompt. Keep pending host input separately until the
next explicit run. No terminal `/goal` workflow, autonomous verifier selection,
or model-facing completion tool is introduced by these SDK methods.

## Limits and accounting

Both budgets are mandatory: `max_prompts` is 1–1024 and `token_budget` is positive.
Defaults are 16 prompts and 100,000 tokens. Objectives, instructions and evidence
are each capped at 16 KiB. Each admitted prompt consumes one prompt allowance
before dispatch, even if execution is later interrupted or saving its intent
cannot be confirmed. Resume does not refund attempts or reset spent tokens.

A “prompt” here includes its native tool-iteration loop, retries and live
follow-ups. It is not a single provider request. Continue configuring the
session's own maximum tool iterations. The token budget stops **subsequent goal
prompts**, not an already-running provider/tool loop. One prompt can overshoot;
usage that the provider never reports cannot be recovered as a billing fact.
Accounting is conservative: for every recorded assistant message, use the larger
of `total_tokens` and the saturating sum of input, output and cache buckets.
This can overestimate when a provider overlaps those buckets.

The timeout starts when `run_goal` or `run_goal_with_verifier` constructs its
future, not at its first poll.
All prompts in that invocation share the same absolute runtime deadline,
including retries, follow-ups and host verification. An inherited deadline can only shorten it.
A new explicit invocation gets a new time allowance, but retains the goal's
spent prompt/token budgets. Expiry requests native abort and drains that same
future; cooperative cleanup may outlive the timeout. Blocking native work and
host callbacks are not forcibly preempted.

Exhaustion is `BudgetLimited`, never automatic completion. A successful final
prompt may still be explicitly verified complete at the budget boundary. An
exhausted goal cannot be resumed by resetting its counters; creating replacement
work requires explicitly dropping the old goal and creating a new objective.

## Pause, input and interruption

`run.control()` returns a handle tied to that run only. `pause()` latches even
between prompts and aborts a currently running prompt. A pause that wins the
verdict-publication lock prevents an already prepared completion from replacing
it. Finished or dropped run handles cannot steer, pause or queue input into a
later run.

`steer()` and `follow_up()` use the native turn-scoped mailboxes. They reject
input when no prompt is accepting it. Input already handed to the agent follows
native history/persistence rules. Still-unclaimed inputs are returned in
`GoalRunReport::pending`, and the driver pauses instead of replaying them into
another goal prompt. `GoalControl::take_pending()` also permits explicit
retraction and recovery after dropping the run. Each input transfers at most
once; manually reclaimed input will not also appear in the report.

An unpolled dropped run changes no goal/session state and dispatches nothing.
Pausing before the first poll also dispatches nothing and leaves the untouched
goal active. Dropping after an intent was appended leaves the goal interrupted;
awaiting cancellation is necessary to observe native cleanup and its outcome.
The driver does not pretend that dropping undoes tool side effects.

## Persistence and reopening

Every lifecycle change uses a bounded `goal_checkpoint` custom entry in the
existing session format. Queries walk the current branch, not global disk
order. Recorded assistant usage since the newest active checkpoint is added
without mutating state; the next checkpoint incorporates it once. Work during
paused/terminal intervals does not charge that goal. Malformed newest goal
records are rejected rather than falling back to an older active objective.

The public methods `current_goal`, `create_goal`, `pause_goal`, `resume_goal`,
`complete_goal`, `drop_goal` and `flush_goal` never start provider work by
themselves. Mutations require a snapshot of the exact storage incarnation,
session identity, current branch leaf and goal revision. A stale UI decision
cannot complete a replacement objective. After reopening, obtain a fresh
snapshot even if the session and goal IDs are unchanged.

Before each goal prompt, the driver appends `inFlight: true`, increments its
prompt count, and saves that intent when the session is persistent. Only a
confirmed save (or an explicitly memory-only session) permits dispatch. If
execution is abandoned, the saved unfinished intent reports interrupted/paused
(or budget-limited when exhausted). Inspect history and side effects before
calling `resume_goal`; work is never replayed automatically on startup.
Explicit resume keeps the objective and spent counters, then continues from the
existing conversation rather than resubmitting the original prompt as a retry.

A live transition and its durable acknowledgement are distinct:
`GoalPersistence::Saved` confirms `Session::save`, `MemoryOnly` identifies an
ephemeral session, and `Unconfirmed` preserves the live change but stops further
work. Retry `flush_goal`, not the already-applied create/complete decision.
Native save guarantees and limitations still apply; this is not a new database,
a crash-atomic transaction spanning tools and session files, or a cross-process
execution lease.

Always inspect the run report. `completion` retains the last native result,
including typed deadline/provider/persistence failures and partial completion.
`error` independently describes a goal-journal or validation failure. A later
journal failure does not erase the original native outcome. `goal` can be stale
when an error prevented a fresh read; get a new snapshot before acting on it.
Objective/evidence and message contents are excluded from the report's Debug
output but remain available through explicit fields and persisted session data;
those files are not a secrets-redaction boundary.

## Validation status

The goal lifecycle and runner include regression tests over native sessions,
local HTTP/SSE requests, saved-session reopening, pause races, budget exhaustion,
input recovery and failed persistence. They were authored but not executed in
the implementation environment because `dsr quality --tool pi_agent_rust` could
not start (`dsr` absent, exit 127). Compilation, tests, rustfmt and Clippy still
require the repository's DSR quality lane. No quality gate was substituted.

Borrowed-session and asynchronous-verification workflows are covered in
`tests/goal_borrowed.rs`; timer/cancellation registration probes are in
`src/goal/driver/verification.rs`. These tests were also authored but not run in
the implementation environment. Source inspection is not runtime acceptance.
