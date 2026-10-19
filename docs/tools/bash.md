# Bash commands and automatic background jobs

The `bash` tool runs a command in the agent's current working directory, streams
progress while it waits, and returns the command's output and exit status. It
uses the configured shell, command prefix, proxy environment, execution
permissions, and Bash mediation policy.

## Choose how the command runs

| Input | Behavior | Default execution timeout |
| --- | --- | --- |
| Omit `background` | Non-PTY commands wait inline, then automatically become jobs after the configured threshold. | 120 seconds, measured from the original spawn |
| `"background": true` | Start a job immediately and return its ID. | 1,800 seconds |
| `"background": false` | Keep the command in the direct foreground runner. | 120 seconds |

Set `"timeout": 0` to disable the execution timeout. A positive `timeout` is
always expressed in seconds. Automatic promotion never restarts the command
or resets its timeout: a command with a 120-second timeout promoted at 60
seconds has about 60 seconds left.

Automatic mode applies only to non-PTY commands. PTY selection still follows
`bash.pty`. If the execution deadline is at or before the automatic threshold,
the command uses the direct foreground runner, because it cannot usefully
continue beyond that threshold.

## Configure automatic backgrounding

Automatic backgrounding is enabled by default with a 60-second threshold:

```json
{
  "bash": {
    "autoBackground": {
      "enabled": true,
      "thresholdMs": 60000
    }
  }
}
```

`thresholdMs` is a nonnegative integer in milliseconds. Zero requests immediate
promotion; values above 600,000 are clamped to 600,000. Global and project
settings merge field by field, so a project can change `enabled` without
discarding a global threshold. The snake-case spellings `auto_background` and
`threshold_ms` are accepted too.

Set `"enabled": false` to keep omitted-`background` calls in the direct
foreground runner. An explicit `"background": true` still starts a job.

## Ownership, completion, and cancellation

The managed process starts once. The foreground tool call owns it until
completion or promotion, and the same process and output pumps continue
after promotion. Fast commands return ordinary inline output and preserve
nonzero exit status; they do not create a completion follow-up.

A promoted result includes `id`, `pid`, `status`, `artifactPath`, output
counters, and `autoBackgrounded: true` in its details. Manage it with:

```json
{"action": "list"}
{"action": "wait", "jobId": "JOB_ID", "timeoutMs": 30000}
{"action": "cancel", "jobId": "JOB_ID"}
```

Those are inputs to the `jobs` tool. Jobs and their completion notices remain
scoped to the session that started them. A terminal job produces one
completion notice for that session. A command that finishes before promotion
stays inline, including when exit and the promotion threshold coincide.

Cancelling or dropping a tool call before promotion requests termination of
its process tree and suppresses a later background notice. A session switch
during the foreground wait also cancels that session's pending command.
After promotion, the job remains owned by its original session and can be
cancelled through `jobs`; owner-scoped shutdown still terminates its jobs.
Timeout and cancellation use the existing TERM/KILL escalation and reap
discipline.

Managed foreground commands share the eight-job concurrency budget with
background commands. If admission is refused because that budget is full,
automatic mode uses the direct foreground runner. That fallback occurs
before any process is spawned, so it cannot replay a partially executed
command. Explicit background requests still return the named capacity error.

## Output retention

Foreground display output keeps the ordinary Bash limit: the last 2,000
lines or 1 MB. Managed commands retain a larger internal tail while they wait
inline. On promotion, the job tail is reduced to 64 KiB; subsequent
`list`, `wait`, and `cancel` snapshots keep that bound. Byte and line counters
remain cumulative, and completion notices include a smaller excerpt.

**Automatic mode uses the native job artifact limit of 16 MiB from the
original spawn, including commands that finish inline.** Its artifact stores
the beginning of the output; the live tail continues advancing after the
artifact reaches its cap. This differs from the direct foreground runner's
1 GiB spill limit. Use `"background": false` or disable automatic mode when
a command needs that larger retained log.

Truncated artifacts and incomplete pipe capture are reported explicitly in
the output and structured details. A partial artifact is labeled with its
real retained path and is not advertised as a complete `fullOutputPath`.
The `capture` details report `outputComplete`, `artifactTruncated`,
`artifactError`, and `artifactPath` for incomplete managed capture. Inspect
the retained artifact for its prefix and the result/job tail for its latest
output.

Each Bash call still starts a shell invocation. This automatic-backgrounding
support does not persist shell-local variables, functions, aliases, or
working-directory changes into later calls.
