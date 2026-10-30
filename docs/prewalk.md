# Prewalk: plan with one model, continue with another

Prewalk is an opt-in, one-shot model handoff. Start with the model you want to
investigate and plan the work. After it creates a todo list (when the `todo`
tool is enabled) and successfully edits a project file, Pi saves the complete
tool batch, selects the destination model, and continues the same conversation.

The destination receives the existing user request, plan, and tool results.
The first edit is not repeated. The original time and tool-iteration limits
continue across the handoff and any provider retries.

## Interactive startup

```sh
# Use prewalk.into (the explicitly configured smol role by default).
pi --prewalk

# Assign a role for this launch and arm it.
pi --smol openai/gpt-5-mini --prewalk

# Override prewalk.into for this launch, optionally including thinking.
pi --prewalk-into openai/gpt-5-mini:low
pi --prewalk-into @smol

# Override automatic startup arming for this launch.
pi --no-prewalk
```

The starting model is selected normally by `--provider`, `--model`, or saved
session settings. `prewalk.from` is used by `/prewalk restart`; it does not
replace the startup model. The destination must resolve to a registered model and have
usable credentials before prewalk is armed. Unknown model names, unconfigured
roles, and disabled destination providers are errors. Prewalk does not change
your role assignments or persist an enabled flag.

To arm prewalk automatically on fresh interactive launches, configure:

```json
{
  "modelRoles": { "smol": "openai/gpt-5-mini" },
  "prewalk": { "enabled": true, "from": "@default", "into": "@smol" }
}
```

The default is disabled. Automatically opening a saved session does not re-arm
prewalk; an explicit startup flag can arm a resumed session. After `/new`,
`/resume`, a fork, or another session replacement, use `/prewalk` to arm the
current session explicitly.

`prewalk.from` and `prewalk.into` accept exact registered `provider/model`
selectors or explicitly configured roles such as `@slow`, `@plan`, and
`@smol`. Both support a `:thinking` suffix. `@default` resolves the configured
default role, falling back to `defaultProvider`/`defaultModel`; a temporary
`--model` selection or a previous handoff does not redefine it. Project
settings override individual prewalk fields while preserving other inherited
fields. Setting the two selectors alone does not enable startup arming.

## Commands

Both interactive frontends provide:

| Command | Effect |
| --- | --- |
| `/prewalk` | Arm `prewalk.into`, or keep the existing armed destination. |
| `/prewalk <into>` | Arm a one-command destination, without changing saved settings. |
| `/prewalk restart` | Return to `prewalk.from` and arm `prewalk.into`. |
| `/prewalk restart <from> [into]` | Use a one-command planning model and optional destination. |
| `/prewalk status` | Show whether a handoff is armed and its destination. |
| `/prewalk off` | Cancel an armed handoff. |

Both frontends resolve roles when the command runs, including CLI role
overrides. The classic frontend also honors live `/model <role> provider/model`
assignments. If no role is configured, choose one before arming. An identical
destination model and thinking level reports that there is nothing to switch.

Restart validates both models, credentials, and workspace provider policy
before changing the session. It saves the planning model and thinking level
together, then arms the handoff. A conflicting already-armed destination is
rejected; use `/prewalk off` before choosing a different destination. A matching
arm is preserved. After a consumed handoff, a new restart cycle requires a new
successful todo when the todo tool is enabled; old todos cannot trigger the
next handoff. Invalid selectors or failed saves leave the prior live selection
and handoff intact. Restart does not submit another user message or replay any
completed tools.

For example, to plan on a powerful role and implement on the usual default:

```text
/prewalk restart @slow @default
/prewalk status
```

The arguments apply to this command only. A later bare restart uses the saved
`prewalk.from` and `prewalk.into` again.

Successful `write`, `edit`, and `hashline_edit` calls trigger the handoff.
Native `conflict://` writes also count when their result confirms applied
project files. Failed or rejected tools, reads, discussion, and plan artifacts
under `plan://`, `.pi/plans`, or `.omp/plans` do not trigger it. Shell commands,
extension-specific mutation tools, and nested tool wrappers are not classified
as prewalk edits.

The handoff waits for every call in the current tool batch. Queued steering is
kept, cancellation prevents another provider request, and a newer explicit
model or reasoning selection takes precedence over the armed destination.
Switching models or thinking manually, or interrupting a turn, disarms prewalk. The selected
model and thinking level are persisted together before the new provider runs.

## Print and RPC

The same explicit launch flags work for text output, JSON event streams, and
RPC servers:

```sh
pi --print --prewalk --smol openai/gpt-5-mini "Implement the requested change"
pi --mode json --prewalk-into openai/gpt-5-mini:low "Implement the requested change"
pi --mode rpc --prewalk-into @smol
```

These modes require an explicit prewalk flag; `prewalk.enabled` only supplies a
default for fresh interactive launches. Add `--session run.jsonl` to save a
print run. Session export does not execute a provider turn and cannot use
prewalk flags.

The handoff runs even when automatic retries are disabled. Destination
failures use the normal retry and compaction policy, while the original
authored input, completed tools, time limit, and tool-iteration budget are
retained. JSON and RPC emit `prewalk_switched` after the saved model transition.
RPC keeps live steering admission open through the handoff and emits its
terminal `agent_end` when the entire request finishes. Explicit RPC model or
thinking selections disarm the pending launch handoff.

## In-process SDK

Set `SessionOptions.prewalk` when creating an SDK session, or arm an existing
handle:

```rust,ignore
use pi::prewalk::PrewalkTarget;

handle.arm_prewalk(PrewalkTarget {
    provider: "openai".into(),
    model: "gpt-5-mini".into(),
    thinking: None, // inherit the current level, clamped to the destination
}).await?;

let answer = handle.prompt("Implement the requested change", |event| {
    // AgentEvent::PrewalkSwitched is emitted after the durable transition.
}).await?;
```

The SDK's prompt, image/content, and continuation entrypoints share this
workflow. `prewalk_target()` reports the armed destination and
`disarm_prewalk()` cancels it. `restart_prewalk(source, target)` validates and
durably selects the planning model before arming a new cycle; both arguments
are `PrewalkTarget` values. A logical SDK turn still emits one `AgentStart`
and one `AgentEnd`. The low-level `Agent` loop does not perform a handoff by
itself. Native `AgentSession` drivers can share the same durable transition:
call `begin_prewalk_turn()` once per logical request, retain its token through
retries, and check `prewalk_handoff_pending()` after a successful saved attempt.
Call `complete_prewalk_handoff()` before continuing the existing transcript,
then `finish_prewalk_turn(interrupted)` on every exit. A newer explicit selection
can win the transition without restarting the authored request. The SDK and
built-in drivers handle this lifecycle automatically.

## Editor sessions over ACP

Configure the editor's agent command with an explicit destination:

```sh
pi --acp --prewalk --smol openai/gpt-5-mini
pi --acp --prewalk-into openai/gpt-5-mini:low
```

Each newly created or reopened editor session resolves the destination under
its own workspace's provider policy and credentials before admission. A
successful file edit triggers the same durable SDK handoff; the editor receives
a status notice and updated model/thinking selectors. Tool permissions, live
steering, retries, and cancellation keep their normal behavior.

The launch flags explicitly arm each new session owner. Reattaching an already
live editor session keeps its current prewalk state. The `prewalk.enabled`
setting does not implicitly arm ACP sessions, and `--no-prewalk` disables launch
arming. ACP's built-in tool set does not include `todo`, so an approved successful
project edit can trigger the handoff without a todo prerequisite.

Behavior reference: [OMP v18.14.0 prewalk documentation](https://github.com/can1357/oh-my-pi/blob/v18.14.0/docs/prewalk.md).
