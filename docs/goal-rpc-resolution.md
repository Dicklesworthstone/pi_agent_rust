# Reviewed RPC goal completion and save recovery

RPC hosts can explicitly attest that a goal is complete, and retry persistence
without repeating a lifecycle decision. These are native extensions of the
existing OMP-shaped `goal` command. They do not execute an objective or install
a completion verifier, and assistant prose cannot call them automatically.

## Read, review, and resolve

Read the selected branch's state:

```json
{"id":"review","type":"goal","op":"get"}
```

The response's data includes `revision`, beginning with `gr1-`. After reviewing
the actual deliverables, send that exact receipt on the existing guarded stop
route:

```json
{"id":"finish","type":"goal","op":"pause","revision":"<revision from get>","resolution":{"kind":"complete","evidence":"I inspected the changes and the recorded test results."}}
```

`resolution` describes the requested stop outcome. Its presence is explicit;
a plain `op:"pause"` keeps its prior behavior. Keeping this on the pause route
preserves the outer RPC loop's idle-state and returned-input recovery preflight.
It does not add a new mutation command that bypasses that preflight. Resolve
returned input through the ordinary recovery workflow before resolving a goal.

The completion response has `goal.status:"complete"`, `state.enabled:false`,
the exact evidence in `goal.note`, a fresh revision, and
`resolution:{"kind":"complete","journalChanged":true}`. The same goal ID,
objective, spent tokens, started prompts, and total allowances are retained.
Completion appends one native checkpoint; it does not first append a redundant
pause. Evidence is a host-authored attestation, not independent proof. It is
never evaluated as a command or inferred from the assistant's response.

Unfinished native intents cannot be completed. Reconcile interrupted work
through the existing lifecycle before attempting completion. Already completed
or dropped goals reject another completion. An old receipt cannot complete a
replacement goal, another session, another branch, or changed usage. The receipt
is a logical revision identifier, not a credential or permission grant.

## Recover an unconfirmed save without replaying completion

A lifecycle decision and its disk acknowledgement are separate. Always inspect
`persistence`: it is `saved`, `memory_only`, or `unconfirmed`. An unconfirmed
response keeps the live decision and includes `persistenceError`. Do not repeat
the completion request: it has already changed the in-memory journal.

After fixing the storage problem, get and review the current state again, then:

```json
{"id":"save","type":"goal","op":"pause","revision":"<current revision>","resolution":{"kind":"save"}}
```

This saves the existing native session without appending a goal checkpoint,
pausing/resuming the goal, resetting budgets, or replaying model/tool work.
Its `resolution.journalChanged` is false. The goal, evidence, selected branch,
and revision remain the same. Save-only recovery works for active, paused,
budget-limited, complete, dropped, and interrupted goals; interrupted work
remains interrupted. A branch without a goal is an error. A stale receipt is
refused before saving. Repeating save with an unchanged receipt is safe, but
it remains a fresh persistence attempt, not a durable exactly-once protocol.

The whole existing session is saved through its normal storage backend. This
operation does not repair a corrupt store, override filesystem permissions,
clear a native provider-admission quarantine, or promise crash durability beyond
the existing session save contract. A cancelled owner or contended session lock
fails without a new goal decision. No provider request is made by either
resolution and no model, tool, approval, or workspace policy is changed.

## Input contract

`resolution` must be exactly one of:

- `{"kind":"complete","evidence":"..."}` with nonblank evidence of at most
  16 KiB UTF-8, preserved literally.
- `{"kind":"save"}` with no evidence or additional fields.

A resolution requires `op:"pause"` and a current, well-formed `revision`.
Null, unknown kinds, extra nested fields, and combinations with `budget`,
`objective`, `token_budget`, `max_prompts`, or `reason` are rejected by the
lifecycle adapter. The outer RPC read-only `get` path remains read-only; use
`pause`, not extra fields on `get`, to request a resolution. Existing plain
pause and reviewed pause/budget requests are unchanged. These payloads do not
add top-level `op:"complete"` or `op:"save"` spellings.

## Validation status

Eleven native AgentSession/goal-journal adapter regressions cover literal
attestation, counters and terminal state, stale and repeated decisions, all
save-only statuses, interrupted intents, malformed/conflicting input, changed
usage and branches, saved-session reopening, save failure and retry, owner
cancellation, lock contention, corrupt state, and existing pause/budget behavior.
They use the actual journal and session persistence, not replacement stores.
They do not constitute an end-to-end RPC subprocess or live-provider test.

These tests were authored but not run on the implementation host.
`dsr quality --tool pi_agent_rust` could not start because `dsr` is absent
(exit 127). Compilation, Rust tests, formatting, Clippy, and platform/runtime
acceptance remain unverified. No direct Cargo/RCH substitute or Actions was used.
The separately prepared terminal goal keyboard-control patch is not included.
