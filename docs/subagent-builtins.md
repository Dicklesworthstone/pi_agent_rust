# Built-in native subagents

The opt-in `subagent` tool now works without installing agent Markdown files.
Its five bundled profiles use the existing native child runner:

| Agent | Purpose | Tools |
| --- | --- | --- |
| `task` | Implementation and multi-step delegated work | read, bash, edit, write, grep, find, ls, hashline_edit |
| `scout` | Locate relevant code, call paths and tests | read, grep, find, ls |
| `reviewer` | Find correctness defects and regressions | read, grep, find, ls |
| `security-reviewer` | Defensive security review of trust boundaries | read, grep, find, ls |
| `sonic` | Narrow mechanical updates or data collection | read, bash, edit, write, grep, find, ls, hashline_edit |

```json
{"agent":"scout","task":"Map the provider failover flow and identify its tests"}
```

```json
{"chain":[{"agent":"scout","task":"Locate the parser and describe the required fix"},{"agent":"task","task":"Implement the fix using this evidence: {previous}","isolation":"worktree","isoApply":"keep"}]}
```

Definitions are resolved in this order: built-in, user, nearest project, with
later definitions replacing earlier definitions of the same name. `scope`
selects which authored overrides are read; built-ins remain available for
`both`, `user`, and `project`. A malformed selected override remains an error,
not an excuse to silently use a different profile. Discovery writes no files.

Profiles have no model pins. They use the host's existing task/smol-role
selection, then the captured active parent model, then ordinary child
configuration. In particular, the `sonic` name does not automatically select a
new model or change thinking effort. Override its Markdown definition to pin
those settings. A user/project model pin retains its existing precedence.

The names match OMP v18.10.4's catalog, but these are native Pi prompts and
policies, not full OMP task-runtime parity. Bundled profiles do not enable
recursive delegation or hub control. The inspection profiles have no shell,
write, eval, or process tools. They inspect source; they cannot run tests.
These tool selections are not an operating-system sandbox or a substitute for
workspace trust policy. Authored overrides may intentionally select other tools.

Parallel and chained calls, structured-output validation and its bounded retry,
request-wide deadlines, inherited cancellation, hub bookkeeping and worktree
acceptance continue through the existing runner. Enabling this catalog does
not enable the opt-in tool globally and does not create or publish Git commits.

## Single-task workspace selection

Single-task calls now accept the same `cwd`, `isolation` and `isoApply` options
as entries in `tasks` and `chain`:

```json
{"agent":"task","task":"Implement the parser fix and run the permitted checks","cwd":"packages/parser","isolation":"worktree","isoApply":"keep"}
```

`cwd` is absolute or relative to the host workspace. In worktree mode its
repository-relative location is retained inside the captured checkout; a
missing/non-Git workspace fails instead of running in another directory.
`keep` retains accepted edits for inspection, `apply` uses existing serialized
writeback, and `drop` reports the patch without applying it. Omitting all three
options retains the previous unisolated host-directory default.

An explicit `isoApply` requires `isolation: "worktree"`. Nulls, wrong types,
unknown policy values, empty paths and NUL-containing paths are rejected
before agent discovery or process launch, including SDK calls that bypass the
tool JSON Schema. For `tasks`/`chain`, workspace options must be placed on each
entry; top-level options are rejected rather than ignored or broadcast. These
checks do not add path confinement or change authored-agent discovery roots.
