# Typed native subagent definitions

Put a named agent in `$PI_CODING_AGENT_DIR/agents/*.md` or the project's
`.pi/agents/*.md`. Existing discovery precedence remains built-in, user, then
nearest project. A malformed discovered definition is an error, not permission
to substitute a built-in or a more permissive tool list. Two files declaring
the same name in one directory are rejected; a project definition may still
override a user definition.

```markdown
---
name: reviewer-json
description: >-
  Review the proposed change
  and return a machine-readable verdict.
tools:
  - read
  - grep
  - find
skills: ['review notes, v2.md']
structured: true
output: |
  {
    "type": "object",
    "required": ["verdict", "findings"],
    "properties": {
      "verdict": {"enum": ["pass", "fail"]},
      "findings": {"type": "array", "items": {"type": "string"}}
    },
    "additionalProperties": false
  }
---
Inspect the actual source. Report concrete defects, not hypothetical style concerns.
```

Delegate with `{"agent":"reviewer-json","task":"Review the parser change",
"schemaMode":"strict"}`. The existing native runner supplies the schema directive,
validates the final JSON, and exposes parsed `data`, `schemaValid` and the bounded
corrective retry count. Invalid output after that retry fails a strict task;
`schemaMode: "permissive"` retains its existing warning behavior. A task's explicit
`outputSchema` still overrides the definition's schema.

## Per-call model selection

A single delegation, each `tasks`/`chain` entry, and each workflow `call` may
select its own model. This lets a cheap mechanical agent and a deeper reviewer
use different destinations without maintaining duplicate agent definitions:

```json
{
  "tasks": [
    {"agent": "sonic", "task": "Update the named references.", "model": "openai/gpt-5-mini:low"},
    {"agent": "reviewer", "task": "Inspect the parser for correctness defects.", "model": "anthropic/claude-sonnet-4-6:high"}
  ]
}
```

Use an exact `provider/model` selector, optionally followed by `:thinking`, or
an exact model ID that belongs to only one provider. Available thinking names
are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; the selected
model must support the requested level. A registered literal model ID wins
before suffix parsing, preserving local IDs such as `vendor/model:1`.

Precedence is **per-call model → definition model → task role → smol role →
captured parent → ordinary child configuration**. An explicit thinking suffix
overrides the definition's `thinking`; omitting it preserves existing effort
selection. Unknown or ambiguous models, missing configured credentials,
disabled providers and unsupported effort fail before any child, isolated
worktree, or background acknowledgment. Batch-wide `model` is rejected: put it
on each task. Null, arrays, patterns and role aliases are rejected too.

Admission uses the host's effective provider policy and the longest matching
workspace scope at both host and task directories. As with explicit CLI model
selection, `enabled_models` is a cycling/startup preference; provider disables
remain enforced. Catalog lookup includes native models, `models.json`, and
validated persisted model catalogs, without remote discovery or extension
initialization. Extension-only registrations must first be represented in the
native catalog to use this override. Credential commands resolve once per
provider during admission; child startup still loads its own model metadata and
credentials. The frozen model selection carries only provider, model ID, and optional
thinking level, so changing the parent's model cannot reroute queued work,
schema retries, workflow descendants, or explicit hub revival.

`worker_start` and `pool_start` accept the same `model` field and freeze it for
the entire retained conversation or pool. `worker_send` and `pool_push` cannot
change it. Eval factories forward it in both languages:

```javascript
const reviewer = await agent('Review the parser.', {
  agent: 'reviewer', model: 'anthropic/claude-sonnet-4-6:high', keepAlive: true
});
const pool = await workpool({agent: 'sonic', model: 'openai/gpt-5-mini:low'});
```

```python
reviewer = await agent('Review the parser.', agent='reviewer',
                       model='anthropic/claude-sonnet-4-6:high', keep_alive=True)
pool = await workpool(agent='sonic', model='openai/gpt-5-mini:low')
```

Saved-session `seance` consultations retain their separate saved-branch model
selection contract; these ordinary task defaults do not replace it.

## Role-owned recovery

When the `task` role selects a child model, that child uses
`retry.fallbackChains.task`. If `smol` supplies the model instead, it uses
`retry.fallbackChains.smol`. An absent role chain inherits the `default` chain;
an explicitly empty role chain (`[]`) disables fallback for that role. Role
ownership stays attached after automatic model changes, including prewalk and
earlier fallback hops. A per-call `model` or definition `model:` uses the default
policy even if it happens to name the same model as a configured role.

```json
{
  "modelRoles": {"task": "openai/gpt-5-mini:low"},
  "retry": {
    "fallbackChains": {
      "default": ["anthropic/claude-sonnet-4-6:high"],
      "task": ["google/gemini-3-flash:low"],
      "smol": []
    }
  }
}
```

Fallback entries accept `provider/model[:thinking]`. Each hop applies its own
effort, clamped to the destination's supported levels; restoring the primary
restores its original effort. Registered literal IDs take precedence over suffix
parsing. Repeating the same model with a different effort does not spend another
fallback hop.

Admission freezes the host's retry controls, chains, selected role, and provider
exclusions from both the host and task workspace. The snapshot follows queued
tasks, parallel and sequential batches, workflow descendants, schema retries,
hub revival, `/tan`, and the entire lifetime of a retained worker or pool.
Changing settings after admission or entering an isolated worktree does not
replace it. A successful explicit model selection inside a child retires role
ownership while retaining its host exclusions. Saved-session consultations use
the default policy with their saved or explicit model. The process handoff is
bounded to 32 KiB and contains no credentials; oversized policy is rejected
before launching work.

## Accepted frontmatter

This is a bounded **agent-definition subset**, not a general YAML implementation
or complete OMP agent-file compatibility. Supported fields are `name`,
`description`, `model`, `thinking` (or `reasoning`), `tools`, `skills`,
`structured`, and `output` (or `output_schema`). `name` and `description` are
required. Unknown fields, duplicate fields and simultaneous aliases are errors,
so unsupported execution policy cannot disappear silently.

Strings accept plain, single-quoted or double-quoted values. Double quotes use
JSON escapes; single quotes escape an apostrophe by doubling it. Comments begin
with `#` outside quotes after whitespace. A hash inside a string remains data.
Literal `|`, `|-` and `|+` blocks preserve line boundaries. Folded `>` variants
support consecutive plain lines; paragraphs and more-indented folded content
are refused with guidance to use a literal block. Headers may use CRLF and an
initial UTF-8 BOM. The Markdown body after the closing delimiter is retained
without reformatting, including its final newline.

`tools` and `skills` accept the native comma-separated string, a bracketed string
list, or an indented `- string` list. Use lists for skill paths containing commas.
Paths remain relative to the definition file unless absolute; they are not
interpolated or fetched. List members must be nonempty strings. Nulls, numeric
or boolean members, nested collections and malformed lists fail instead of
being dropped or converted into tool names. Quote literal numeric/boolean names.

**Omitted tools and empty tools have different meanings.** Omission keeps the
existing native defaults. `tools: []`, an empty string, or a blank `tools:`
selects no built-in tools. That choice reaches child argv through `--no-tools`
as well as an empty `--tools` value; it never falls back to the default list.
This is a built-in tool selection, not an OS sandbox or a change to extension,
MCP, workspace-trust or host approval policies.

`output` and `output_schema` are aliases for a JSON Schema object or boolean.
Use inline JSON or a literal block containing JSON; arbitrary indented YAML
schema mappings are not accepted by this initial implementation. Schemas are
compiled at definition load time, before a child is launched. Parser and schema
errors identify the file and failure category without echoing schema values.

`structured: true` keeps native JSON validation active independently of the
optional structured-result text footer. Without an explicit schema it selects
an unconstrained JSON schema: any JSON value is allowed, ordinary prose is not.
`structured: false` with an explicit schema is rejected as contradictory, rather
than silently choosing which declaration to ignore. These are native final-output
semantics; no OMP `yield` tool or new child runtime is introduced, and this field
does not override the task's strict/permissive failure policy.

## Bounds and validation

Each definition is a regular UTF-8 file of at most 1 MiB, with a header of at most
64 KiB. Each directory admits at most 256 Markdown files, and each tools/skills
list at most 128 entries. Unix reads are nonblocking before checking the opened
file type so a FIFO named `.md` cannot stall discovery. Authored file symlinks
remain supported; these checks are not an atomic snapshot or a new trust boundary.
Tags, anchors, merge keys, includes and executable constructors are unsupported.

Regression coverage lives in `src/subagents/definition/tests.rs`, including three
public `SubagentTool` child-process protocol fixtures for schema validation,
explicit no-tools arguments and refusal before launch. Fixtures do not contact a
model. They are authored tests, not proof that the Rust integration has run.
Acceptance uses the repository's authoritative quality entrypoint:

```sh
dsr quality --tool pi_agent_rust
```
