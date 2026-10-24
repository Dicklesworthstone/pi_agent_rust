# Live stream rules

Stream rules correct a response while the model is generating it. A rule has a
regular expression and a directive. When assistant text or thinking matches,
Pi closes that provider stream, discards its unfinished response, inserts the
directive, and requests a replacement from the preceding conversation.
Tool calls from the discarded response are never executed. Tool argument JSON
is excluded from matching; use tool approval and execution policy for actions.

Rules are enforced in the shared agent loop used by interactive, print, RPC,
and SDK sessions. Providers that return a complete response without text deltas
are checked before their tool calls are admitted too.

## Create and manage a rule

```sh
pi rules add \
  --id no-box-leak \
  --name 'Use owned memory' \
  --pattern 'Box::leak' \
  --body 'Use owned values or scoped references instead of Box::leak.'

pi rules test no-box-leak 'let value = Box::leak(boxed);'
pi rules list --global
```

In the interactive editor, `/rules` lists rules, `/rules toggle no-box-leak`
enables or disables one, and `/rules remove no-box-leak` removes it. `/omfg
<complaint>` records a grievance and derives a candidate rule from it; inspect
the generated expression and test it against representative text. It uses
literal complaint words, so a manually chosen pattern is often more precise.
Failed rule reads or saves are reported explicitly.

Project rules live in `.pi/stream-rules.json`. Add `--global` to `pi rules add`
to write to the configured agent directory, normally
`~/.pi/agent/stream-rules.json`. `PI_CODING_AGENT_DIR` selects that directory in
the same way it selects the other agent settings.

```json
{
  "version": 1,
  "rules": [
    {
      "id": "no-box-leak",
      "name": "Use owned memory",
      "pattern": "Box::leak",
      "body": "Use owned values or scoped references instead of Box::leak.",
      "enabled": true
    }
  ]
}
```

A project rule replaces a global rule with the same ID. A disabled project rule
also suppresses the corresponding global rule. Project rules require workspace
trust; untrusted projects use only global rules. `--no-context-files` disables
ambient stream rules along with context-file loading.

The source is refreshed at new prompt boundaries, so a saved change applies to
the next turn without restarting Pi. A malformed file stops admission with a
named error instead of silently disabling the policy. Imports validate the
complete input before saving any rules. Each scope file is limited to 1 MiB;
at most 128 effective rules are supported, with patterns up to 4 KiB and
directives up to 16 KiB. Invalid and oversized compiled expressions are rejected.

## Corrections, limits, and history

Pi allows three rule corrections per logical turn. Another match stops that
turn with `PI_STREAM_RULE_LIMIT`, preventing an endless retry loop. A provider
retry does not replenish the allowance. Optional `cooldownTurns` suppresses a
rule for the rest of the current turn and that many subsequent turns after a
match; omit it when every attempt must comply. Unchanged rules retain their
cooldown when files are refreshed.

The transcript records the rule ID, name, directive, and injection count.
Discarded assistant text and unfinished tool arguments are excluded from the
saved correction. Already displayed streaming text cannot be recalled from
external consumers, and work completed before the interrupted response remains
completed. The normal turn persistence transaction saves the correction with
the rest of the turn.

Compaction retains the newest correction per rule on the active branch, up to
the 128 most recently triggered distinct rules, independently of the generated
summary. Corrections removed by a checkpoint rewind or belonging to a sibling
branch are not restored. The full append-only session tree still retains its
historical records.

## Embedding

`Agent::set_stream_rules(Vec<StreamRule>)` validates and installs an explicit
rule set. `Agent::set_stream_rule_source(StreamRuleSource::new(project_root,
global_root, project_trusted))` installs a refreshable source. Both return
errors for invalid rules and leave the preceding rule set intact on failure.
Direct `Agent` instances have no ambient rule source unless the host installs
one; the CLI and `create_agent_session` provide the normal configured source.

These are correction rules for model output, not a sandbox or a replacement
for tool permissions. Cancellation still stops a retry, and the ordinary
provider, privacy, approval, and tool policies remain in effect.
