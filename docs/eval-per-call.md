# Fresh per-call eval kernels

Use `kernel_mode: "per_call"` to run a Python or JavaScript cell in a fresh
private interpreter instead of the conversation's retained one:

```json
{
  "kernel": "python",
  "kernel_mode": "per_call",
  "code": "import json\nwith open('measurements.json') as source:\n    values = json.load(source)\ndisplay({'total': sum(values)})",
  "timeout_secs": 30
}
```

Omitting `kernel_mode`, or setting it to `session`, preserves the existing
retained behavior. Unknown values and `null` fail before execution admission or
reset. This is the native per-call workflow inspired by OMP's kernel modes,
not a new Python runner or a claim of full OMP compatibility.

## What is private

Every per-call request gets fresh variables, Python imports/event-loop state,
JavaScript lexical bindings/module cache, and managed Python package storage.
It cannot read or reset either retained session kernel. `reset: true` applies
only to the already-private executor; it does not mean "reset my session".
Ordinary exceptions, hard stops and abandoning a per-call future likewise
leave the parent's interpreter slots and session cache alone. The private
kernel is retired before the result is returned. Cell numbering starts at 1.

The selected workspace and captured Python-selection configuration are copied,
including `EvalTool::new_with_python` overrides. Configuration is not recaptured
from the process environment. Normal fresh-kernel discovery/probing still runs;
there is no copying of Python objects or silently falling back to another
interpreter. Managed `%pip` installs live only until this private kernel ends.

This is **interpreter-state isolation, not a security sandbox**. Filesystem,
network, subprocess and library effects have their normal authority. Files
written by a cell remain written; no rollback, worktree or filesystem copy is
provided. Do not treat arbitrary code as trusted merely because its state is
not retained.

## Execution and results

The private executor uses the ordinary compiler, source loader, invocation-owned tool
bridge, native output capture, protocol validation, deadlines and process/worker
ownership. Calls still share the original EvalTool's execution admission,
including SDK calls: per-call does not enable uncontrolled parallel spawning.
The same live conversation resolver rejects queued or active work after an
observed identity change. Foreground startup, source preparation and callbacks
retain the existing cooperative boundaries.

Progress and final details include `kernelMode: "per_call"` and
`stateRetained: false`. Other metadata, structured JSON/PNG displays, native
Python runtime provenance and optional console archives remain available.
Archives are still explicit, create-only and validated by the normal publisher.
A clean Python soft timeout remains an error, but its private kernel is retired
rather than made reusable; `stateReset` is true and a notice makes that distinction
explicit. Session-mode soft interruption behavior is unchanged.

## Validation

Regressions in `src/eval/isolation/tests.rs` and `tests/eval_per_call.rs` exercise
retained/private state separation, module caches, interpreter configuration,
admission, cancellation, abandoned futures, session changes, callbacks, soft
stops, reset, bridge output and source errors. They are authored Rust tests,
not executed acceptance evidence: `dsr quality --tool pi_agent_rust` could not
start in the implementation environment because DSR is absent (exit 127).
Compilation, Rust formatting, Clippy, native runtime and cross-platform
acceptance remain unverified. No legacy gate or dependency was changed.
