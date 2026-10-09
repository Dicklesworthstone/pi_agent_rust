# Explicit session workdir recovery

The ordinary CLI and SDK resolve a saved session's workspace before loading
project configuration or constructing tools and extensions. The
`session_workdir` library module and developer example also provide read-only
inspection and an explicit locate-and-attach flow for moved projects.
The recovery path covers startup, explicit attachment, forks, and session
discovery. Executable validation remains pending.

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
commands, new-session headers, prompt files, stored credential commands, and
MCP credential helpers use the selected workspace. Credential reloads after
login/logout, model selection, resource reload, usage queries, and SDK fallback
selection retain that runtime workspace. A caller-supplied fallback auth store
keeps its credentials and storage path while inheriting the active runtime's
command context.

Bedrock providers bind their request-time credential reloads to the runtime's
auth-file path and command workspace. Each request reloads credential entries
from that file, so rotation, login, and logout remain visible to ordinary turns,
compaction, checkpoint summaries, and auxiliary clients. Native startup, model
switches, fallback and restore paths pass their selected auth source into
provider construction. ACP scopes credentials to each editor session's cwd;
memory reflection uses the project's root.

The binding belongs to the constructed provider. Programmatically replacing
an `AgentSession` auth store with a different file affects subsequent provider
construction; an existing Bedrock provider continues to reload its original
source. Standalone callers can use `providers::create_provider_with_auth` or
`BedrockProvider::with_auth_storage` with a scoped `AuthStorage`. The original
`create_provider` API retains its standalone behavior.

Forks created through classic, FTUI, and RPC inherit the original cwd,
additional roots, direct parent-session provenance, and the latest workdir
binding. A lazy source is hydrated on an independent copy, including fork
targets outside the loaded branch. An off-branch binding remains metadata and
does not change the fork's selected conversation or editor prefill. Classic
and RPC refuse a fork before saving or installing it if a newly discovered
attachment conflicts with the active runtime workspace.

## Discovery after attachment

`--continue`, `--resume`, and project session pickers group sessions by their
latest saved workspace attachment. A reattached session remains in its original
storage folder and can be found from the replacement workspace even when that
workspace has no encoded session folder. It no longer appears in the original
workspace's project list.

Managed saves update this effective workspace in the index using the accepted
persisted entries, including bindings outside the active branch. JSONL and
SQLite metadata reads use the same binding rules. Older index rows are rebuilt
under the new grouping semantics; when the index is unavailable, discovery
scans the managed session namespace without requiring that cache.

Listing does not require the bound workspace to remain available. Launch still
checks its health. Malformed JSON and invalid or unsupported attachment
metadata are refused during discovery instead of selecting an older binding.
Use explicit-path audit tooling to inspect damaged sources.

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
an older binding. Fork creation preserves this metadata independently of the
selected conversation ancestry.

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
The auth adoption regression executes real credential commands in distinct
workspaces and checks active and fallback credentials through logout/relogin.
The Bedrock wire regression binds a saved workspace and a relative custom auth
file, rotates its command credential, changes the isolated caller's cwd, and
checks an auxiliary summary's actual HTTP authorization. It also checks refusal
of a missing credential workspace. ACP construction regressions check distinct
command credentials for two editor sessions while preserving the host context.
Fork regressions cover saved/reopened context, root forks with empty history,
off-branch lazy targets and bindings, actual classic/FTUI/RPC entrypoints,
source immutability, and refusal of a stale runtime workspace.
Discovery regressions cover effective-workspace indexing, continuation and
picker lookup without a replacement storage folder, stale index migration,
unavailable index fallback, lazy off-branch bindings, and malformed metadata.
They were **not executed in the authoring environment** because DSR was absent.
No compilation, formatting, Clippy or test pass is claimed.

Suggested focused DSR lanes:

```sh
dsr wrap cargo test --lib session_workdir::tests
dsr wrap cargo test --test session_workdir_recovery
dsr wrap cargo test --features sqlite-sessions --test session_workdir_recovery
dsr wrap cargo test --test session_index_tests
dsr wrap cargo test --features sqlite-sessions --test session_index_tests
dsr wrap cargo test --test provider_bedrock_streaming
dsr wrap cargo test --example session_workdir
dsr wrap cargo fmt --check
```
