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

The default terminal binds the handle to its shared runtime control fence and
temporary progress observation. Escape pauses the whole invocation, including
the gaps between prompts. Ctrl-C also pauses an active goal without clearing
the ordinary draft; modal extension/ask/login editors keep their keyboard
ownership. Terminal shutdown seals publication and pauses a goal even if its
native control arrives after shutdown began. A failed progress-channel send pauses that invocation,
sealing continuation and asking any live native turn to abort and drain.
Abandoning the observation also requests pause. Publication after retirement
immediately pauses the incoming control. Control calls happen after releasing
the display mutex so a wakeup can emit events without a lock inversion.
Normal completion only retires the observation; it does not cancel another run.

A deliberate second Ctrl-C within the normal exit window still quits while a
paused goal drains; the first tap retains the draft and its exit timestamp.

Keyboard cancellation reaches the owning `GoalControl` immediately, even when
the provider is silent. Channel-loss cancellation still observes failed sends;
it is not a periodic liveness probe. Progress and final reports carry both
session and invocation identities. A late report cannot end the display of a
new goal, another session or a later ordinary driver command.

Enter steers the current native prompt. Alt+Enter and `/queue` enqueue a
follow-up. File references are prepared once and their ordered native text,
image and media blocks enter the same bounded mailbox as SDK input. The
original authored text remains separate from expanded attachment contents.
Admission in a startup/verifier gap or after pause fails visibly and keeps the
draft; it never silently starts an ordinary prompt after the goal finishes.
Manual `/compact` and `/shake` require pausing the goal first: their ordinary
turn barriers cannot take ownership of a goal driver's inner prompt.

## Attachment-aware SDK goal input

`GoalControl` now exposes the current native turn's input operations directly:

- `steer_with_content` and `follow_up_with_content` accept `UserContent` plus
  original authored prose. Ordered text, image and audio/video blocks are not
  flattened, decoded, expanded or fetched by the control layer.
- `enqueue_batch_with_content` admits `(InputKind, UserContent, String)` tuples
  all-or-nothing to one turn. This makes admission atomic, not execution; a
  native turn may consume only part of an accepted batch before stopping.
- `replace_pending` and `replace_pending_with_content` correct one unclaimed
  input in place. Its lane and queue position are preserved, and its new ID
  retires the old ID. Failure leaves the original untouched. `None` means no
  pending match, not permission to resend already-delivered input.
- `retract` recovers one input by ID without draining its neighbours.
  `turn_snapshot` reports current native queue counts/bytes, acceptance and
  handoff counts without exposing text. These are per-turn observations,
  not goal-lifetime totals or durable receipts.

These methods reuse the existing native mailbox and all its validation,
count/byte limits, authored-keyword boundaries and ownership rules. They do
not grant tool permissions or promise any provider's image/audio/video support.
Before a native turn is installed, and during verifier gaps, input fails with
`GOAL_INPUT_IDLE` rather than being silently queued for some future prompt.
Paused or finished goals reject admission and correction. Snapshot observations
can race completion; operation results are authoritative.

Pause and abandonment still allow recovery. A dropped goal retains its closed
native mailbox through the retained control handle, so `retract`/`take_pending`
return complete payloads once, without making a second recovery queue or
allocating payload copies during drop. Abandonment requests native abort after
releasing the goal mutex; a host/executor waker can inspect the same control
without reentering a held goal lock. Awaiting remains necessary for completed
async cleanup or persistence.

After an awaited run, unclaimed payloads belong to `GoalRunReport::pending`,
not simultaneously to its control handle. After an abandoned run, recover them
from the retained `GoalControl`. An old ID or control cannot retract, correct
or cancel a replacement run's input. No recovery operation automatically
resends anything or resets the goal's prompt/token allowances.

The default terminal uses these same typed APIs for goal input and Alt+Up
recall. It does not add an independent execution queue or change goal budgets.

## Explicit restoration of returned input

The terminal retains the complete native payload on completion, failure and
abandonment, including when the final goal receipt cannot be read. It restores
the authored text only into the owning session's ordinary empty composer;
existing drafts and login, ask, extension and picker input remain untouched.
Alt+Up can recall unclaimed input during the same goal. Recovered attachments
are kept as captured bytes, not reconstructed by rereading file references.

Sending an unchanged recovered draft explicitly resubmits its exact native
blocks and authored text. During its original goal it returns to that goal's
current accepting turn; after settlement it starts one ordinary controlled
turn, with a driver check of the originating session and branch. It never
implicitly resumes a goal. An earlier goal's payload cannot bypass that check
by entering a newer live goal or ordinary turn. Media-only input can be sent
even when its authored text is empty. If a prepared draft is edited, Alt+Up
restores the original; Ctrl-C clears it to compose a new message. This preserves
captured attachments without guessing how edited prose maps to expanded files.

Both `SessionControlHandle` and `GoalControl` provide:

```rust
pub fn restore_pending(
    &self,
    pending: &mut Vec<PendingInput>,
) -> Result<Vec<TransferredInput>>;
```

This accepts a detached batch returned in `GoalRunReport::pending`, by
`take_pending()`, or assembled from successful `retract()` calls. It is an
explicit host decision after reviewing the previous native result, not an
automatic retry. Choose an accepting destination turn; for goals, explicitly
resume/run the reviewed goal first and restore through the control for its
current native prompt. An idle, paused or finished goal cannot be implicitly
resumed by restoration.

On success, the source vector is empty, the destination contains the entire
batch after existing queued input, and receipts map each previous ID to its
new ID in admission order. Steering/follow-up lanes, ordered text/image/media
blocks and original authored prose survive. Old IDs cannot edit or retract the
new entries. Repeating the operation on the now-empty vector admits nothing.
An empty batch is a no-op only when the destination is still accepting input.

On validation, duplicate-ID, closed-destination, allocation or count/byte
capacity failure, both the source vector and destination queue remain unchanged.
There is no accepted prefix that the host must guess how to retry. Source
payloads are temporarily moved into an exclusively borrowed staging view;
refusal or unwinding restores them without rereading files or allocating payload
copies. Only validated, bounded payloads are copied into native batch admission.
All receipt allocation precedes publication.

`PendingInput` exposes editable `text` and `content` fields. Restoration therefore
uses ordinary native batch validation again and recomputes byte accounting;
it never trusts the private size recorded before a host edit. Duplicate source
IDs fail with `SESSION_CONTROL_DUPLICATE_INPUT` before admission. Existing
native limits still apply: 100 pending inputs, 8 MiB total accounted bytes,
4 MiB serialized native content per item, 128 blocks and 256 KiB authored text.
Invalid or oversized content is retained with the caller for correction.

Queue admission is atomic; execution is not. A turn may consume part of an
accepted batch before cancellation. These receipts do not acknowledge provider
execution or durable persistence. A host can clone a `PendingInput` itself, so
this is not a durable exactly-once or cross-invocation deduplication service.
Never reconstruct unclaimed input from already-delivered transcript messages.
Restoration does not dispatch a model request, reset goal counters or authorize
another goal-driver prompt. The existing `transfer_pending_to` API remains the
separate zero-copy path between a finished native mailbox and an accepting one.

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

Seven further tests in `src/goal/driver/input.rs` cover native mailbox sharing,
attachment preservation, atomic invalid/full-batch refusal, in-place correction,
closed/stale controls, concurrent recovery, retained goal-report payloads and
actual native follow-up delivery through a controlled Provider boundary.
The image/audio data in mailbox fixtures is structural test input, not live
media-decoding evidence. The required DSR command was retried for this change
and again could not start (`dsr: command not found`, exit 127). These seven
Rust tests, compilation, formatting and Clippy remain unexecuted.

Detached restoration adds seven native mailbox regressions in
`src/session_control/recovery.rs` for full-batch acceptance, identity rotation,
capacity/closed refusal, edited-content revalidation, duplicate IDs, source
restoration on unwind and byte/count limits. Two further goal tests cover
idle/closed refusal without consumption and an actual goal report whose pending
content is explicitly restored into a later resumed goal's native prompt.
All nine additional Rust tests are authored, not executed. DSR was retried and
remained unavailable (exit 127); no compilation, formatting, Clippy, live-model,
PTY or cross-platform pass is claimed.
