# Prompt Templates

Prompt templates allow you to define reusable prompts with arguments.

## Locations

Templates are Markdown files loaded from:

1. **Global**: `~/.pi/agent/prompts/*.md`
2. **Project**: `.pi/prompts/*.md`
3. **Packages**: Installed packages can also provide templates.

## File Format

A template is a Markdown file, optionally with YAML frontmatter.

```markdown
---
description: "Review code for security issues"
---
Review the following code for security vulnerabilities. Focus on XSS and SQL injection.

Code:
$1
```

If description is omitted, the first line of the file is used. The filename (without extension) becomes the command name (e.g. `review.md` -> `/review`).

## Invocation

Call templates using `/` followed by the name:

```bash
/review src/main.rs
```

Arguments are split on spaces; single or double quotes keep arguments together:

```bash
/review "src/main.rs src/lib.rs" --strict
```

## Side Commands

Set the top-level Boolean `side: true` to answer a template through the same
ephemeral side-question workflow as `/btw` in either terminal interface. For
example, save `.pi/prompts/sitrep.md`:

```markdown
---
description: Summarize current progress
side: true
---
Summarize the current work and the next decision. $ARGUMENTS
```

Invoke `/sitrep`, `/sitrep "focus on retry behavior"`, or
`/template sitrep "focus on retry behavior"`. The template expands arguments
normally, and its answer is labelled with the command you typed. The command
is available in editor recall, but the question and answer are display-only:
they do not enter the main model's conversation, steering queue, or saved
session transcript. The side request uses the configured smol model
(`--smol` or `model_roles.smol`), shares `/btw`'s privacy screening, and has no
tools. Requests made during a turn wait for its live context and privacy state
to become available.

If the side model is unavailable, or the expanded question is empty or too
large, Pi retains the editor draft and shows an error. It never sends a
rejected side command as an ordinary prompt. Native and extension commands
retain ownership of their names; use a distinct template name to avoid a
collision. `/reload` refreshes both the template body and its side setting.

Only a Boolean `true` enables this behavior; quoted `"true"`, `false`, and
omitted `side` fields do not. Outside interactive mode, the same file remains
an ordinary prompt template and expands through the normal prompt path.

## Expansion Syntax

Templates support bash-like variable expansion:

| Variable | Description |
|----------|-------------|
| `$1`, `$2`, ... | Positional arguments |
| `$@`, `$ARGUMENTS` | All arguments joined by spaces |
| `${@:N}`, `$@[N]` | Arguments from index N onwards (1-based) |
| `${@:N:L}`, `$@[N:L]` | Slice of L arguments starting at N |

Missing positional arguments expand to an empty string. Placeholders are
filled in one pass: an argument that contains `$1` or `$@` is inserted as
written.

A template with no placeholder at all still receives its arguments: they
are appended after a blank line, so `/review focus on error paths` on a
template that never mentions `$@` does not lose the request.

### Example: Commit Message

`commit.md`:
```markdown
Write a commit message for the following changes.
Context: $1
Diff:
${@:2}
```

Usage:
```bash
/commit "Refactor auth" src/auth.rs src/main.rs
```

Expands to:
```
Write a commit message for the following changes.
Context: Refactor auth
Diff:
src/auth.rs src/main.rs
```
