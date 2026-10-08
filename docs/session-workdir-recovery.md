# Explicit session workdir recovery

The ordinary CLI and SDK resolve a saved session's workspace before loading
project configuration or constructing tools and extensions. The
`session_workdir` library module and developer example also provide read-only
inspection and an explicit locate-and-attach flow for moved projects.
This implements part of **bd-yutps**; the bead remains open pending executable
validation and the remaining discovery and child-session integration.

## Ordinary CLI and SDK use

```sh
# Resume in the saved workspace, even when invoked from another directory.
pi --session /absolute/session.jsonl

# Explicitly persist a replacement workspace, then resume there.
pi --session /absolute/session.jsonl --session-workdir /new/project

# Existing selectors resolve once, before project startup.
pi --continue
pi --resume
```

`--session-workdir` requires an existing explicit `--session`. A missing target,
an invalid session source, or a failed save prevents launch. The flag cannot be
combined with ephemeral sessions, ACP, export, or model-listing modes. Without
that flag, an unavailable saved workdir produces a recovery error; it does not
silently select the invocation directory.

Relative session locators, session storage directories, and attachment targets
are resolved from the invocation directory. Project resources, prompt files,
MCP configuration paths, additional roots supplied with `--add-dir`, and model
tool paths are resolved from the selected workspace. Startup applies trust and
project settings to that workspace. The process cwd itself remains unchanged.

SDK callers can omit `SessionOptions.working_directory` when resuming an
existing `session_path`; the saved attachment then selects the workspace.
Supplying `working_directory` or a primary `workspace` root asserts that it
matches the saved attachment. It does not grant permission to replace the
attachment. Saved additional roots are restored when no workspace handle was
supplied; unavailable roots are reported and skipped. An explicitly supplied
handle remains authoritative, including live removal of a root.

Classic and RPC live session switches require the target attachment to match
the current runtime workspace. FTUI replacement sessions carry the same
explicit workspace assertion. A refused switch preserves the active session;
launch a new `pi --session ...` process to move to another project. RPC shell
commands, new-session headers, prompt files, startup credential-command lookups,
and MCP credential helpers use the selected workspace. Bedrock's provider-owned
request-time reload of legacy `auth.json` command credentials still needs the
runtime workspace threaded through it.

Recent-session indexing under a replacement workspace and automatic
propagation into newly created child sessions still require integration.

## Developer recovery example

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

Bindings are session-wide, using the latest attachment even when it belongs
to another branch. Saved metadata discovery scans the original store rather
than relying on a lazily loaded V2 tail. Native loading then validates the
transcript, and startup checks that the source still matches the observation.
An attachment request hydrates a lazy store before determining its previous
binding. A malformed latest attachment is an error, not permission to revive
an older binding. Fork/child creation must also preserve this metadata.

Read-only export/import should remain possible for a missing workspace and
must not require the launch guard. Recovery of a source with parsing warnings
is deliberately rejected by the example; inspect it with the existing
`--session-audit` tooling before attempting a modifying recovery operation.

## Validation status and targets

Regression tests cover workdir classification, moved directories,
wrong-directory rejection, provenance preservation, idempotence, malformed
metadata, symlink aliases, safe launch arguments, and JSONL/SQLite persistence.
Startup regressions exercise real SDK read/write tools, lazy off-branch
bindings, restored root access and revocation, native source-change rejection,
project configuration, explicit CLI attachment, and ephemeral/read-only paths.
Additional regressions cover RPC shell execution and session switching,
classic resume and new sessions, prompt files, and credential command cwd.
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
