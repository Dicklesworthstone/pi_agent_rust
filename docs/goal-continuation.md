# Progress-aware goal continuation

The terminal's explicit `/goal run <review> --steps N` now pauses when successful
tool work stops changing instead of spending all N prompts repeating the same
activity. `/goal step <review>` still permits at most one prompt. Both use the
same native driver as the SDK, keep the existing absolute deadline and durable
intent checks, and leave completion for an explicit user/host attestation.

`N` is an upper limit, not a promise to send N prompts. A text-only answer is a
valid result, but does not independently justify another automatic request.
This policy does not claim to understand whether code is correct or an objective
has actually been achieved.

## SDK entry point

`AgentSessionHandle::run_goal_until_review` works on a borrowed handle;
`GoalSession::run_goal_until_review` is its owned-session convenience counterpart.
After creating or explicitly resuming a goal and checking its persistence result:

```rust
use std::time::Duration;
use pi::agent_cx::AgentCx;
use pi::goal::{GoalRunReport, GoalSnapshot};
use pi::sdk::{AgentSessionHandle, Result};

async fn run_reviewed_goal(
    handle: &mut AgentSessionHandle,
    reviewed: &GoalSnapshot,
) -> Result<GoalRunReport> {
    let owner = AgentCx::for_current_or_request();
    let run = handle.run_goal_until_review(
        &owner,
        reviewed,
        8,
        Duration::from_secs(15 * 60),
        |_control, _event| {},
    )?;
    // Obtain run.control() before awaiting to expose live pause/steer/follow-up.
    Ok(run.await)
}
```

The invocation accepts 1–32 prompts. Its own step allowance cannot enlarge the
goal's persistent prompt/token budgets. Failed persistence, cancellation,
unclaimed input, and native failures retain the existing driver's precedence.
An unused dropped run performs no provider or goal-journal work. Dropping an
already-started run still has the native cleanup/interruption limitations.

The existing `run_goal` and `run_goal_with_verifier` APIs are unchanged. Use them
when a host has better evidence of progress, wants repeated polling, or must
verify a non-tool-based workflow. Their custom decisions are not silently
filtered by this heuristic.

## What counts as new activity

A completed successful native tool operation contributes a fingerprint of its
name, canonical JSON arguments and visible result blocks. JSON object-key order,
call IDs, provider signatures and diagnostic `details` do not alter that
fingerprint. Image/media bytes are included without decoding them. Tool outputs
are neither truncated nor retained by the activity history.

Every next prompt must contribute at least one successful operation/result pair
not already seen in this invocation. The comparison is independent of parallel
completion order and multiplicity. Repeating A, then B, then A stops; so does
reordering a batch or duplicating one of its operations. A changed argument or
changed visible result can count as new activity. Failed tool operations cannot
independently authorize another prompt, even when their errors keep changing.

This remains a heuristic. Timestamp-bearing visible output or meaningless
argument changes can look new. Conversely, a deliberately repeated successful
check may be useful despite identical output. The user can inspect the result
and explicitly resume, or the SDK host can supply a domain-specific verifier.
No hash or heuristic is treated as proof of completion or permission to expand
tool authority.

## Pause reasons and bounds

The standard report retains the last native completion. The goal's note carries
one of these stable reason codes, also shown by `/goal status`:

| Code | Meaning |
| --- | --- |
| `GOAL_NO_TOOL_PROGRESS` | No completed successful tool operation was observed. |
| `GOAL_REPEATED_TOOL_PROGRESS` | All successful operations/results have already appeared in this run. |
| `GOAL_PROGRESS_UNCERTAIN` | Incomplete, mismatched or over-budget observation; no partial prefix is accepted as progress. |
| `GOAL_STEP_LIMIT` | The user-requested prompt allowance is consumed. |

A step-limit pause takes precedence when the requested final prompt completes.
The shared driver may instead report cancellation, a native error, a persistent
goal budget limit, or unconfirmed saving. Inspect the complete report, not just
its last assistant text.

Each prompt observes at most 256 calls, 4 MiB of charged payload, 16,384 JSON
nodes, argument depth 32, and 1,024 blocks per result. IDs and tool names are
bounded before copying. Fingerprinting streams borrowed data, sorts only bounded
object-key references, and keeps fixed-size fingerprints rather than output
copies. If any observation is incomplete or exceeds these limits, the entire
observation becomes ineligible to authorize continuation. The native prompt is
allowed to finish; this is not an additional preemptive tool-execution limit.

History is local to one explicit invocation. Reopening or explicitly resuming
and starting another run creates a fresh activity history, while the goal's
persistent spent budgets remain. Nothing automatically resumes on startup.

## Verification status

The implementation includes focused observer tests and native HTTP/SSE/read-tool
integration tests. They are authored but not executed in this environment:
`dsr quality --tool pi_agent_rust` could not start because `dsr` was absent (exit
127). Compilation, Rust test execution, rustfmt and Clippy still require the
repository's normal quality lane; source checks are not a substitute.
