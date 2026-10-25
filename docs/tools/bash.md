# Bash commands, persistent shell state, and background jobs

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

## Commands run without a terminal

Nobody types into a command the agent runs, so every runner (pipes, PTY and
jobs) sets a non-interactive environment, as OMP does:

- `PAGER`, `GIT_PAGER`, `MANPAGER`, `GH_PAGER` and the other tool pagers are
  `cat`, `AWS_PAGER` is empty, and `LESS` is `FRX`.
- `EDITOR`, `VISUAL` and `GIT_EDITOR` are `true`, so `git commit` without
  `-m` stops with "empty commit message" instead of waiting in an editor.
- `GIT_TERMINAL_PROMPT=0` and `GH_PROMPT_DISABLED=1`.
- `SSH_ASKPASS=false` with `SSH_ASKPASS_REQUIRE=force`, so ssh reports a
  password, passphrase or unknown-host-key prompt as a failure.

These replace the same variables from pi's own environment. An assignment
in the command itself, such as `EDITOR=vim git commit`, still wins.

With `bash.pty` on `auto`, pagers (`less`, `more`, `man`) run on pipes
and print their text. On a PTY nobody could page through or quit them.

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

## Persistent shell state

On Unix, native Bash calls retain shell state within the agent session by
default. This applies to direct foreground calls, PTY calls, and commands
that automatically become background jobs. A new call can use variables,
indexed and associative arrays supported by the configured Bash version,
functions, aliases, shell options, umask, and the working directory saved
by the preceding call. For example, these two tool calls share state:

```json
{"command":"BUILD_TARGET=release; build_target() { printf '%s' \"$BUILD_TARGET\"; }; cd src","background":false}
```

```json
{"command":"build_target; pwd","background":false}
```

State is scoped to the captured agent session, configured Bash binary,
command prefix, and initial workspace directory. Rebuilding a tool registry
does not discard it. A session switch cannot import another session's state.
Other file tools continue to use their own workspace directory. If a saved
directory is renamed or removed, the next Bash call returns to its initial
workspace directory and retains the other saved state.

Only one call owns a session's shell state at a time. An automatically
promoted command keeps that ownership until it settles, and its final state
is published before `jobs.wait` reports completion. Calls that overlap it
run in temporary shells at the initial workspace directory. Their results
include a warning and `details.shellState.mode: "temporary"`; saved session
variables and functions are unavailable, and those calls do not change the
saved state. Explicit `background:true` jobs always use independent shells
and do not occupy the session shell.

The implementation transfers native Bash declarations between supervised
processes. It executes each user command once; it does not replay earlier
commands to reconstruct state. A completed command saves state even when
its final command returns nonzero. A shell that terminates before reaching
normal completion—for example through `exit`, `exec`, `set -e`, or a
`set -u` error—retains the previous committed state. Timeout and cancellation
also retain the previous state. This differs from OMP's embedded interpreter,
which can retain changes made before interpreter-level `exit` or errexit.
The configured command prefix still runs for each call.

State is held in process memory and transferred through private temporary
files, separate from tool output artifacts. Each snapshot is capped at
1 MiB; incomplete or oversized snapshots preserve the preceding snapshot.
The pool retains at most 32 session/configuration combinations, evicting
the oldest idle combination when needed. State is not serialized into saved
agent transcripts, and restarting Pi starts a fresh shell state. Process
IDs, open file descriptors, traps, and shell job tables do not carry across
calls. Bash's special runtime variables are regenerated for each process.
Windows and explicitly configured non-Bash shells retain their existing
execution behavior.

Set `"bash": {"persistent": false}` to run independent foreground shells.
This leaves previously saved state available if persistence is re-enabled.
