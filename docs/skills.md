# Skills

Skills provide specialized instructions and capabilities to the agent. They are defined using the [Agent Skills](https://github.com/Start-Agent-Skills) format.

## Locations

Pi automatically discovers skills in both Pi-specific and universal locations:

| Scope | Pi-specific directory | Universal directory |
|-------|-----------------------|---------------------|
| Project | `.pi/skills/` | `.agents/skills/` |
| User | `~/.pi/agent/skills/` | `~/.agents/skills/` |

Pi-specific project skills are relative to the current working directory.
Universal project skills are discovered from `.agents/skills/` in the current
working directory and each ancestor directory, stopping at the nearest Git root
(or at the filesystem root when no Git root exists). At most 64 ancestor
locations are probed. Discovery checks only those bounded candidate roots; it
does not recursively scan unrelated directories.
`PI_CODING_AGENT_DIR` changes the Pi-specific user directory; the universal user
directory remains `.agents/skills/` under the user's home directory.

Place universal skills in their own directories, such as
`.agents/skills/sql-expert/SKILL.md`. Universal roots follow the interoperable
Agent Skills convention and ignore root-level Markdown files; Pi-specific roots
continue to support their existing root-level Markdown form. Discovery is
recursive within each skill root and skips hidden entries and `node_modules`.

Skills can also come from `--skill` paths, `skills` entries in settings, and
packages. Agent-authored managed skills live in the configured agent directory's
`skills.managed/` directory.

Missing default directories are silently ignored. Symlinked skills are supported;
the same canonical file is loaded once, and directory cycles do not cause
unbounded traversal. Each selected skill root has a 16,384-entry discovery
budget. A root that exceeds the budget is rejected as a whole rather than
returning a filesystem-order-dependent partial catalog. Unreadable, oversized,
invalid UTF-8, and missing-description skill files are skipped with diagnostics
while valid skills continue to load.

### Precedence

During session startup and resource reload, the first skill with a given name
wins, using this order:

| Priority | Source |
|----------|--------|
| 1 | Explicit `--skill` paths, in command-line order |
| 2 | Skills supplied by CLI `-e` sources |
| 3 | Project `skills` settings entries and `.pi/skills/` |
| 4 | Project `.agents/skills/`, nearest ancestor first through the Git/filesystem root |
| 5 | User `skills` settings entries and `<agent_dir>/skills/` |
| 6 | User `~/.agents/skills/` |
| 7 | Installed project packages |
| 8 | Installed user packages |
| 9 | Managed `<agent_dir>/skills.managed/` |

`<agent_dir>` defaults to `~/.pi/agent/` and follows `PI_CODING_AGENT_DIR`.
Configured resources within a tier use deterministic path order. Different
files declaring the same name produce collision diagnostics that identify both
the winning and shadowed paths. Aliases of the same canonical file do not
produce a name collision.

For example, `.agents/skills/sql-expert/SKILL.md` overrides a user-level
`sql-expert`, while `.pi/skills/sql-expert/SKILL.md` overrides the project's
universal copy. `--skill /path/to/sql-expert` overrides both.

## File Format

A skill is defined in a `SKILL.md` file with YAML frontmatter.

```markdown
---
name: "sql-expert"
description: "Expert at writing and optimizing SQL queries"
disable-model-invocation: false
---
You are an expert SQL developer. When writing queries:
1. Always prefer CTEs over subqueries.
2. Use uppercase for keywords.
3. Check for index usage.
```

### Frontmatter Fields

| Field | Description |
|-------|-------------|
| `name` | Skill ID (must match directory name if in a subdir; mismatches emit a warning). |
| `description` | **Required.** Short description used for selection; empty descriptions are skipped. |
| `disable-model-invocation` | If `true`, the skill is not shown to the model in the system prompt. |

If `name` is omitted, the parent directory name is used.

## Usage

### Auto-Discovery

By default, Pi includes all enabled skills in the system prompt. The model can decide to "activate" a skill by reading its definition file using the `read` tool.

### Explicit Invocation

You can explicitly invoke a skill using the slash command:

```bash
/skill:sql-expert "Optimize this query..."
```

This effectively wraps your prompt with the skill's instructions.

## Configuration

To disable the `/skill:` slash commands, set `enable_skill_commands` to `false` in `settings.json`.

`--no-skills` disables automatic discovery, configured `skills` entries, installed
package skills, and managed skills. Explicit `--skill` paths and skills supplied
by CLI `-e` sources still load. For example, an isolated skill set can be selected
with:

```bash
pi --no-skills --skill /path/to/sql-expert --skill /path/to/code-review
```

The existing `skills` exclusion and override patterns also apply to universal
directories. Project discovery follows the same workspace-trust and
`PI_CONFIG_PATH` rules as `.pi/skills/`.
