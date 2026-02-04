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

## Expansion Syntax

Templates support bash-like variable expansion:

| Variable | Description |
|----------|-------------|
| `$1`, `$2`, ... | Positional arguments |
| `$@`, `$ARGUMENTS` | All arguments joined by spaces |
| `${@:N}` | Arguments from index N onwards (1-based) |
| `${@:N:L}` | Slice of L arguments starting at N |

Missing positional arguments expand to an empty string.

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
