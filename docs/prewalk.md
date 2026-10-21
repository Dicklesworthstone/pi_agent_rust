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
# Use the explicitly configured smol role as the destination.
pi --prewalk

# Assign a role for this launch and arm it.
pi --smol openai/gpt-5-mini --prewalk

# Choose a registered destination directly, optionally including thinking.
pi --prewalk-into openai/gpt-5-mini:low
pi --prewalk-into @smol

# Override automatic startup arming for this launch.
pi --no-prewalk
```

The starting model is selected normally by `--provider`, `--model`, or saved
session settings. The destination must resolve to a registered model and have
usable credentials before prewalk is armed. Unknown model names, unconfigured
roles, and disabled destination providers are errors. Prewalk does not change
your role assignments or persist an enabled flag.

To arm prewalk automatically on fresh interactive launches, configure:

```json
{
  "modelRoles": { "smol": "openai/gpt-5-mini" },
  "prewalk": { "enabled": true }
}
```

The default is disabled. Automatically opening a saved session does not re-arm
prewalk; an explicit startup flag can arm a resumed session. After `/new`,
`/resume`, a fork, or another session replacement, use `/prewalk` to arm the
current session explicitly.

## Commands

Both interactive frontends provide:

| Command | Effect |
| --- | --- |
| `/prewalk` | Arm the `smol` role, or keep the existing armed destination. |
| `/prewalk status` | Show whether a handoff is armed and its destination. |
| `/prewalk off` | Cancel an armed handoff. |

The default frontend uses the `--smol` launch override or `modelRoles.smol`.
The classic frontend also honors a live `/model smol provider/model` role
assignment. If no role is configured, choose one before arming. An identical
destination model and thinking level reports that there is nothing to switch.
After a handoff, `/prewalk` can arm another one.

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
`disarm_prewalk()` cancels it. A logical SDK turn still emits one `AgentStart`
and one `AgentEnd`. The low-level `Agent` loop does not perform a handoff by
itself; use the owning SDK handle so persistence and model installation stay
together.

Print, RPC, and ACP startup currently reject explicit prewalk CLI flags. They
do not silently accept a workflow those drivers have not enabled.

Behavior reference: [OMP prewalk documentation](https://omp.sh/docs/prewalk).
