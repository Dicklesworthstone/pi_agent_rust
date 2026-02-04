# Settings

Pi reads JSON settings and applies them with clear precedence rules.

## Locations

Pi loads settings from (up to) two files:

| Location | Scope |
|----------|-------|
| `~/.pi/agent/settings.json` | Global (all projects) |
| `.pi/settings.json` | Project (current directory) |

You can override the path entirely with `PI_CONFIG_PATH` (see below).

Run `pi config` to print the effective paths and precedence.

## Precedence (highest → lowest)

1. CLI flags
2. Environment variables
3. Project settings (`.pi/settings.json`)
4. Global settings (`~/.pi/agent/settings.json`)
5. Built-in defaults

## `PI_CONFIG_PATH` (single-file mode)

If `PI_CONFIG_PATH` is set, Pi loads *only* that file and skips the global/project merge.

## Merge behavior (global vs project)

Project settings override global settings on a per-field basis.

Important detail: nested objects like `compaction`, `retry`, `images`, `terminal`, `branch_summary`,
and `thinking_budgets` are treated as *single* fields. If `.pi/settings.json` contains a
`compaction` object, it replaces the entire global `compaction` object.

Within a single file, missing nested keys fall back to built-in defaults when accessed (see
`src/config.rs`).

Example:

```json
// ~/.pi/agent/settings.json (global)
{ "compaction": { "enabled": false, "reserve_tokens": 16384 } }
```

```json
// .pi/settings.json (project)
{ "compaction": { "reserve_tokens": 8192 } }
```

Resulting behavior:
- `compaction.reserve_tokens` becomes `8192`
- `compaction.enabled` does **not** inherit `false` from global; it falls back to its built-in default

## Supported settings (snake_case JSON keys)

### Appearance

- `theme` (string): Theme name to apply. Defaults to `dark` if unset.
- `hide_thinking_block` (bool): Hide thinking blocks in interactive output. Default `false`.
- `show_hardware_cursor` (bool): Show terminal hardware cursor. Default `false` unless
  `PI_HARDWARE_CURSOR=1`.

### Model selection

- `default_provider` (string)
- `default_model` (string)
- `default_thinking_level` (string)
- `enabled_models` (array of model patterns)

Example:

```json
{
  "default_provider": "anthropic",
  "default_model": "claude-sonnet-4-20250514",
  "default_thinking_level": "medium",
  "enabled_models": ["claude-*", "gpt-*"]
}
```

### Message delivery (queue modes)

- `steering_mode` (string): `one-at-a-time` or `all` (default `one-at-a-time`).
- `follow_up_mode` (string): `one-at-a-time` or `all` (default `one-at-a-time`).

Legacy aliases: `steeringMode`, `followUpMode`.

```json
{
  "steering_mode": "one-at-a-time",
  "follow_up_mode": "one-at-a-time"
}
```

### Interactive UX / editor

- `double_escape_action` (string): `tree` or `fork` (default `tree`).
  Alias: `doubleEscapeAction`.
- `editor_padding_x` (u32): Horizontal editor padding (clamped to 0–3). Default `0`.
- `autocomplete_max_visible` (u32): Max autocomplete rows (clamped 3–20). Default `5`.
- `session_picker_input` (u32): Non-interactive session picker selection (1-based).
  Alias: `sessionPickerInput`.
- `quiet_startup` (bool): Suppress the startup header.
- `collapse_changelog` (bool): Condense “What’s New” output when present.

### Compaction (defaults)

Accessor defaults:
- `compaction.enabled`: `true`
- `compaction.reserve_tokens`: `16384`
- `compaction.keep_recent_tokens`: `20000`

```json
{
  "compaction": {
    "enabled": true,
    "reserve_tokens": 16384,
    "keep_recent_tokens": 20000
  }
}
```

### Branch summary

- `branch_summary.reserve_tokens` (u32): Defaults to `compaction.reserve_tokens`.

### Retry (defaults)

Accessor defaults:
- `retry.enabled`: `true`
- `retry.max_retries`: `3`
- `retry.base_delay_ms`: `2000`
- `retry.max_delay_ms`: `60000`

```json
{
  "retry": {
    "enabled": true,
    "max_retries": 3,
    "base_delay_ms": 2000,
    "max_delay_ms": 60000
  }
}
```

### Shell

- `shell_path` (string): Shell binary path. Default `/bin/bash`.
- `shell_command_prefix` (string): Default `set -e`.
- `gh_path` (string): Override path to `gh` for `/share`. Alias: `ghPath`.

```json
{
  "shell_path": "/bin/bash",
  "shell_command_prefix": "set -e"
}
```

### Images

- `images.auto_resize` (bool): Default `true`.
- `images.block_images` (bool): Default `false`.

```json
{
  "images": {
    "auto_resize": true,
    "block_images": false
  }
}
```

### Terminal display

- `terminal.show_images` (bool)
- `terminal.clear_on_shrink` (bool)

### Thinking budgets (tokens)

- `thinking_budgets.minimal`: default `1024`
- `thinking_budgets.low`: default `2048`
- `thinking_budgets.medium`: default `8192`
- `thinking_budgets.high`: default `16384`
- `thinking_budgets.xhigh`: default `u32::MAX` (effectively “no cap”)

### Packages and resources

- `packages` (array): package sources (string or `{ source, local, kind }`).
- `extensions`, `skills`, `prompts`, `themes` (arrays): resource filters.
- `enable_skill_commands` (bool): default `true`.

## Unimplemented or partially wired settings

These settings are defined in `src/config.rs` but are not fully wired into behavior yet:

- `quiet_startup`, `collapse_changelog` → tracked by `bd-35y0` and `bd-217r`.
- `session_picker_input` → tracked by `bd-14cc`.
- `terminal.show_images`, `terminal.clear_on_shrink` → tracked by `bd-xk3g`.

## Full reference

`src/config.rs` is the authoritative list of supported fields and defaulting behavior.
