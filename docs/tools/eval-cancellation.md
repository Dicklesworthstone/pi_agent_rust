# Eval cancellation and host-wait deadlines

An eval call retains the caller's `AgentCx` through admission, file preparation,
execution, and synchronous retirement. This applies to both Python and JavaScript,
including direct SDK calls that bypass the agent's tool scheduler.

Waiting for the exclusive kernel lease and reading a JavaScript `%load` source
are cancellable without running code or discarding retained state. They do not
consume the active cell timeout. A requested reset is applied only after admission
and source preparation succeed. A blocking source read already dispatched may
finish after its caller is cancelled, but cannot subsequently execute the script.

The active cell's `timeout_secs` also covers time awaiting host-side tool reads.
Previously the receive-loop watchdog stopped being polled while the host awaited
a bridge tool, so that tool could outlive the cell budget. The enclosing watchdog
now remains active across the complete Python/JavaScript operation. It does not
start a new timeout window for each tool call.

Owner cancellation is checked before polling, while waiting (20 ms timer ticks),
and before accepting a ready result. A result arriving after cancellation or the
deadline is not reported as success. Polling and dropping native operations use
the same captured owner context; that context is not held across executor polls.

Stopping an active operation drops its checked-out kernel and retires any kernel
returned to the slot during a late completion. Only the selected language loses
state. Python reports a tool error; JavaScript preserves its existing unsuccessful
`ToolOutput` convention. Both disclose `EVAL_STATE_RESET`; timeout uses
`EVAL_TIMEOUT`, and owner cancellation uses `EVAL_CANCELLED`. JavaScript watchdog
results add `details.stateReset: true` and an `errorKind` of `timeout` or `cancelled`.
Normal backend errors and successfully returned rich displays retain their
existing contracts. Interrupted, unreturned output captures may be lost.

These are cooperative async boundaries, not hard real-time preemption. A single
synchronous syscall/poll or process-tree cleanup can delay observation. Existing
process guards own Python cleanup; JavaScript worker revocation uses its existing
interrupt/deadline mechanism. A host operation already admitted may complete, and
cancellation does not roll back file changes or other completed side effects.

Regression tests in `src/eval/lifecycle.rs` cover unstarted/late-ready operations,
resource retirement, queued reset cancellation, active Python/JavaScript state
loss, per-language isolation, and exclusion of queued time from the cell budget.
The authoritative validation entry point remains `dsr quality --tool pi_agent_rust`.
