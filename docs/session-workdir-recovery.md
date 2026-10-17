# Explicit session workdir recovery

The `session_workdir` library module and `session_workdir` developer example
provide an explicit inspect / locate-and-attach / resume / start-new flow for
sessions whose workspace moved, disappeared, or is no longer accessible.
This is an implementation slice of **bd-yutps**, not closure of the whole bead.

## Current integration boundary

The frontend is an example executable, not an additional shipping binary.
The ordinary `pi --session`, startup picker, in-app resume and RPC routes do
**not yet call the new guard**. Use the recovery frontend to get its guarded
launch behavior. Recent-session indexing under the new workspace and automatic
propagation into newly created child sessions still require integration.

## Build and use

All Cargo work must use the repository's DSR validation/build route:

```sh
dsr wrap cargo build --example session_workdir
```

Once the example executable is available locally:

```sh
# Read-only JSON: originalCwd, boundCwd, and structured health.
./target/debug/examples/session_workdir inspect /absolute/session.jsonl

# Locate the replacement yourself; this command is explicit consent to attach.
# It persists an attachment, but does not run a model or an extension.
./target/debug/examples/session_workdir attach /absolute/session.jsonl /new/project

# Resolve the Pi executable before selecting the child's working directory.
./target/debug/examples/session_workdir resume /absolute/session.jsonl \
  --pi-binary ./target/debug/pi

# Abandon the resume intention, NOT the file. No file is deleted or overwritten.
./target/debug/examples/session_workdir start-new /new/project \
  --pi-binary ./target/debug/pi
```

Session selectors are exact paths here, not prefixes or fuzzy search. A relative
user-supplied path is resolved from the invocation directory. Recorded relative
workdirs are not guessed; they require explicit attachment. Paths with spaces
or shell metacharacters are passed as arguments, never interpreted by a shell.

The child Pi process starts in the selected workdir before it reads project
configuration, trust settings, skills or extensions. The helper never changes
its own process cwd. It rejects unavailable workdirs and rechecks before spawn;
it does not promise descriptor-level protection against concurrent directory
replacement. It requires an explicit Pi executable rather than relying on a
changed-directory PATH lookup.

## Persistence and provenance

An attachment is a normal custom session entry with type
`pi.session.workdir.v1` and `originalCwd`, `previousCwd`, and `cwd` fields.
The original header cwd, session ID, parent-session provenance and storage path
are not rewritten. Existing JSONL and optional SQLite persistence handle the
entry; save failure prevents a success response. Equivalent canonical paths
are idempotent, including symlink aliases.

Bindings are session-wide, using the latest attachment in a fully hydrated
session. A malformed latest attachment is an error, not permission to revive
an older binding. Serialization retains the entry. Fork/child creation and
bounded-hydration callers must deliberately preserve/load the binding before
using the guard; these routes have not been retrofitted in this slice.

Read-only export/import should remain possible for a missing workspace and
must not require the launch guard. Recovery of a source with parsing warnings
is deliberately rejected by the example; inspect it with the existing
`--session-audit` tooling before attempting a modifying recovery operation.

## Validation status and targets

Regression tests were authored for workdir classification, moved directories,
wrong-directory rejection, provenance preservation, idempotence, malformed
metadata, symlink aliases, safe launch arguments, and JSONL/SQLite persistence.
They were **not executed in the authoring environment** because DSR was absent.
No compilation, formatting, Clippy or test pass is claimed.

Suggested focused DSR lanes:

```sh
dsr wrap cargo test --lib session_workdir::tests
dsr wrap cargo test --test session_workdir_recovery
dsr wrap cargo test --features sqlite-sessions --test session_workdir_recovery
dsr wrap cargo test --example session_workdir
dsr wrap cargo fmt --check
```
