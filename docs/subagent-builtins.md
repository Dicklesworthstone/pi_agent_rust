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

## Background delegation

Add `"async": true` to a single, `tasks`, or `chain` request to release the
calling turn while the same native child runner continues:

```json
{
  "async": true,
  "tasks": [
    {"agent": "scout", "task": "Map provider retry and cancellation behavior"},
    {"agent": "reviewer", "task": "Review the pending parser changes"}
  ],
  "concurrency": 2,
  "timeoutSeconds": 300
}
```

The acknowledgment has `details.schema: "pi.subagent.background.v1"`, a
`jobId`, and its initial `job` snapshot. Manage that ID through the ordinary
`jobs` tool:

```json
{"action":"wait","jobId":"job-…","timeoutMs":30000}
```

`list` discovers jobs in the current session; `cancel` stops unfinished work.
A settled snapshot has `schema: "pi.host_job.v1"` and `result`, containing the
native tool result (`content`, `details`, and `isError` when true). Its
`result.details.results` keeps each task's output, hub ID, typed data, schema
acceptance and worktree outcome. A wait that reaches its own timeout returns a
still-running snapshot; it does not restart or cancel the delegation. Completion
also enters the owning session's existing background follow-up queue.

Agent definitions, model selection and one request deadline are captured before
the acknowledgment. Definition edits or model changes do not retarget queued
work. All batch members must name valid agents and valid output schemas before
any job is admitted. Parallel execution, chain substitution, retries, isolation
and ordered writeback still use the existing runner. Worktree baselines are
captured when that runner begins the request. Session shutdown and explicit
cancellation retire the native future and its unfinished child process leases;
effects already accepted or performed are not rolled back.

Background jobs share the existing eight-job capacity with shell jobs. Their
complete final result is privacy-projected before artifacts, retention or
follow-up delivery. The result has a 1 MiB encoded limit: an oversized or
unprojectable result is replaced by a named failure, never a successful partial
result. These jobs do not publish live partial tool output; child progress and
transcripts remain available through the agent hub after children register.

The `async` field must be boolean. `seance` and dependency-graph `workflow`
requests currently remain foreground and explicitly refuse `async: true`.
No callback, borrowed Agent or eval interpreter is retained by the background
job. Native tests in `src/subagents/background.rs` cover early return, typed
results, child reaping, whole-batch refusal, frozen definitions/models and
session changes/cancellation during admission. Rust acceptance remains
`dsr quality --tool pi_agent_rust`.
