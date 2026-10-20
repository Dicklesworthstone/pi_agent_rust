# Terminal goal execution ownership and outcomes

`/goal step <review>` and `/goal run <review> --steps N` still use the same
native SDK goal driver, branch receipt, total prompt/token allowances and
15-minute invocation deadline. The shared `run_goal_until_review` progress
policy is preserved: N is an upper bound, and absent/repeated successful tool
activity can pause earlier. This change does not select another provider,
change tool approvals, supply a completion verifier or grant extra tool rights.

## Control belongs to one invocation

The terminal entry point publishes that driver's `GoalControl` before polling
it. Receipt, grammar, missing-goal and plan-mode checks happen first. Lifecycle
commands publish no execution control. A host can pause the handle before the
first poll without dispatching a provider request or consuming a prompt intent.
Finished/dropped handles cannot steer or stop a later invocation.

The default workspace dispatcher binds the handle to its existing temporary
progress observation. A failed progress-channel send pauses that invocation,
sealing continuation and asking any live native turn to abort and drain.
Abandoning the observation also requests pause. Publication after retirement
immediately pauses the incoming control. Control calls happen after releasing
the display mutex so a wakeup can emit events without a lock inversion.
Normal completion only retires the observation; it does not cancel another run.

This is **observed channel-loss cancellation**, not a periodic liveness probe.
A silent provider with no event to send may not expose a disconnected UI until
its next event or the existing deadline. This does not yet connect Escape,
Ctrl-C, steering or follow-up entry to the terminal's `TurnControlSlot` for goal
commands. The default terminal still needs that separate keyboard-control
integration. The native controller and its existing SDK APIs do support pause
and live input for hosts that explicitly retain the published handle.

## Reports preserve native outcomes

The final report distinguishes the goal driver's stop from the native turn's
stop reason and error. Provider/transport failures are no longer hidden merely
because `GoalRunReport::error` contains no goal-journal error. Deadline and owner
cancellation reports inspect their retained post-drain completion, including
partial assistant text and native cleanup/persistence errors.

Unsubmitted inputs are labelled as not replayed. The report quotes their text;
it does not authorize another prompt. Large native text is bounded and labelled
when truncated. The final goal snapshot, persistence acknowledgement and fresh
review receipt are separate from that native-output budget. The receipt uses
post-run live plan state instead of reusing the pre-run value.

Status output continues to advertise step/run for active goals and distinguishes
explicit execution from lifecycle commands. A model's prose still cannot
self-complete the goal: bounded terminal continuation ends with Pause, never a
fabricated Complete verdict.

## Validation

The existing terminal journal, progress-policy and progress-display tests remain
in place. `src/goal/terminal/control_tests.rs` adds seven native Agent/Session/
goal-driver cases with a controlled Provider boundary: pre-dispatch pause,
progress-policy/accounting preservation, pause between prompts, native failure
reporting, receipt and plan-mode refusal, retained deadline partial results and
unsent input, and bounded reporting. These are not live-model or PTY tests.

Three additional progress tests use real unpolled GoalRun controls to check
receiver loss, late publication and abandoned-versus-finished observation
behavior. The implementation host attempted `dsr quality --tool pi_agent_rust`,
but `dsr` was absent (exit 127). All ten new Rust tests are authored, not compiled
or executed; formatting, Clippy, native runtime and cross-platform acceptance
remain pending.
