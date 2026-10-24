# Managed Python packages in eval

Opt in to private package storage by putting `--managed` immediately after
`install`. Ordinary `%pip install` is unchanged; see [eval](eval.md):

```json
{"kernel":"python","timeout_secs":120,"code":"%pip install --managed numpy pandas"}
```

Successful installs become available to later cells and `%load` scripts without
changing system Python, the active interpreter, or existing variables. Each
invocation stages a fresh dependency generation and activates its import path
only after pip succeeds. Failed installs leave previous paths/packages active.
Ambient pip configuration is ignored in managed mode, the shared pip cache is
disabled, and pip scratch files remain under the host-owned kernel directory.
The configured interpreter must still have pip; there is no bootstrap/fallback.

Managed options are `--no-index`, `--find-links` (`-f`), `--index-url` (`-i`),
`--extra-index-url`, `--no-deps`, `--pre`, and `--upgrade`. Quote paths and version
constraints. For example:

```json
{"kernel":"python","code":"%pip install --managed --no-index --find-links 'wheel house' 'example==1.0'"}
```

Destinations, alternate interpreters, editable installs, requirements files and
arbitrary pip flags are refused in managed mode. Wheel-only selection is
requested; prepare source-only packages separately. Normal package installation
and import remain code-execution-capable, not a sandbox or a rollback of arbitrary
third-party side effects. The same 16 KiB/128-argument command limits apply.

Installer output uses the existing live capture and optional console archive.
`EVAL_PACKAGES_READY` confirms activation; `EVAL_PACKAGES_UNCHANGED` means markers
selected no packages. Already imported modules remain cached after upgrades;
reload deliberately when supported, or reset and reinstall a coherent set.
Activation never executes `.pth` hooks or adds console scripts to `PATH`.

Managed packages are temporary and private to the retained kernel, not a project
virtualenv. At most 32 successful generations are retained. Python reset,
timeout, crash, eviction or disposal loses them with that kernel's variables.
Other kernels do not inherit their paths. Rust owns storage until the process
tree is reaped, then cleans it up; abrupt termination of the Pi host can leave
temporary files behind. There is no package-size/disk-consumption cap here.
Installation counts against `timeout_secs`, without a paused watchdog.

`tests/eval_packages.rs` adds real offline wheel/dependency workflows, failure
isolation, upgrades, reset/disposal and a stalled loopback index. Python-only
probes are not Rust integration or a passing DSR quality result.
