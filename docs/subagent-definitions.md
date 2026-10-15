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
