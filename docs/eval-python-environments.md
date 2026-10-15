# Python environments for retained eval

Python eval now selects an interpreter before executing the first cell in a
fresh kernel. This closes the project-environment discovery gap relative to
OMP v18.11.0 while preserving native session ownership, output, tool permissions,
and the original whole-cell deadline. No Python installation is performed.

## Selection

`PI_EVAL_PYTHON` is an explicit override. A bare name is searched on the captured
absolute PATH directories. A path is used literally, with `~/` expansion and
relative paths resolved against the eval workspace. Spaces do not need embedded
shell quoting; arguments, shell expressions, and activation commands are not
interpreted. An empty override is an error: unset it to enable discovery.
Non-UTF-8 executable paths remain native paths rather than being lossily decoded.

Without an override, fresh kernels check, in order:

1. `VIRTUAL_ENV` and then `CONDA_PREFIX`.
2. The workspace's `.venv` and then `venv`.
3. `python3`, then `python`, on absolute PATH directories.

Unix environments use `bin/python` and `bin/python3`. Windows venvs use
`Scripts/python.exe`; Windows Conda uses `python.exe` at the environment root.
Empty and relative PATH entries are not used for executable discovery. Duplicate
entry paths are checked only once. Interpreter symlinks are NOT canonicalized:
Python needs the venv entry path to identify its environment correctly.

Selection inputs are captured when the EvalTool is constructed. Environment
files can be created later, before the first cell or an explicit reset. Already
retained kernels keep their interpreter and variables; restoring a conversation
restores that kernel's selection too. Reset or a fresh kernel reruns discovery.
There is no process-global interpreter cache and no fallback to another language.

Missing candidates are skipped. A candidate that starts but fails its Python
3.10+ version/startup check, or takes more than two seconds to finish the check,
is skipped during automatic discovery. Explicit selection never falls through
to a different interpreter after a started probe fails. Spawn errors other than
not-found, including permission/policy denial, stop discovery rather than
searching around that denial. A broken non-executable environment must therefore
be repaired or explicitly overridden.

## Environment and provenance

The selected executable's directory is prepended to the child PATH. Selected
venvs receive `VIRTUAL_ENV`, selected Conda receives `CONDA_PREFIX`, and the other
prefix is removed. `PYTHONHOME` is removed for discovered environment roots;
`PYTHONPATH` and unrelated variables remain unchanged. Windows Conda also
prepends `Library/bin` and `Scripts`. These are interpreter-selection essentials,
not execution of activation scripts or a promise of full shell activation.
Explicit paths do not infer or rewrite environment-prefix variables.

The probe and actual kernel use the same command environment. Both set
`PYTHONUNBUFFERED=1` and `PYTHONIOENCODING=utf-8`. Eval result details expose the
retained selection on success and ordinary Python errors:

```json
{
  "pythonRuntime": {
    "executable": "/workspace/.venv/bin/python",
    "source": "project-dot-venv"
  }
}
```

Sources are `explicit`, `active-venv`, `conda`, `project-dot-venv`, `project-venv`,
and `path`. A diagnostic path may use replacement characters when the native
path is not Unicode; execution never uses that diagnostic representation.

## Execution and failure boundaries

Probes run fixed version-check code, not the user's cell. They have null stdin,
stdout, and stderr, and use the same native process-group/Windows Job ownership
as the kernel. Their owned trees are retired before selection is returned.
Cancellation, session changes, and the hard cell deadline remain in force during
discovery. The candidate list is bounded to 128 and PATH to 64 KiB; discovery
also has an eight-second budget. These are cooperative host checks, not a
preemptive bound on OS process creation, filesystem calls, or cleanup.

Probe success is not executable attestation or proof of every runner dependency.
The actual kernel bootstrap and existing protocol validators still have to
succeed. Discovery can execute code from local environment executables and their
ordinary Python startup hooks; it is not a sandbox for an untrusted workspace.
It does not verify package isolation or strip application credentials.

After an observed idle process exit, a fresh kernel can be selected before
sending a new request. If request writing fails, delivery is uncertain: the
kernel is retired with `EVAL_STATE_RESET`, and the cell is NOT automatically
replayed or retried on another interpreter. Completed effects are not rolled
back. The existing hard/soft deadline, managed `%pip`, display, and bridge
contracts are otherwise unchanged. There is no global managed venv or package
installation added by discovery; `%pip install --managed` retains its existing
kernel-local generation storage.

## Validation

`src/eval/python_runtime/tests.rs` contains native unit and public EvalTool
scenarios for precedence, literal/non-Unicode paths, command environment,
limits, cancellation/session refusal, real venv-only imports, retained state,
reset, and explicit failure without cell effects. These Rust tests were authored
but not executed in the implementation environment. The required
`dsr quality --tool pi_agent_rust` could not start (DSR absent, exit 127).
Compilation, Rust formatting, Clippy, native-host runtime and Windows acceptance
remain unverified. Standalone real Python environment checks validate the
probe/venv assumptions only; they do not execute the Rust resolver.
