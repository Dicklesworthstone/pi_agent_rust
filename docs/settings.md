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

- `theme` (string): Theme to apply. Built-ins: `dark`, `light`, `solarized`; also
  accepts a discovered theme name or a path to a theme JSON file. `light/dark`,
  `auto`, or `system` auto-detect dark/light from the terminal background via the
  `COLORFGBG` environment variable (dark when unavailable). If unset, auto-detects
  the same way.
- `hide_thinking_block` (bool): Hide thinking blocks in interactive output. Default `false`.
- `show_hardware_cursor` (bool): Show terminal hardware cursor. Default `false` unless
  `PI_HARDWARE_CURSOR=1`.

### Model selection

- `default_provider` (string)
- `default_model` (string)
- `default_thinking_level` (string)
- `enabled_models` (array of model patterns)

`default_model` accepts either a bare model ID or `provider/model`. Pi first
tries to resolve the complete provider/model pair. When it exists, that
provider takes precedence over `default_provider`. For example:

```json
{
  "default_provider": "polza",
  "default_model": "routerAI/deepseek/deepseek-v4.1-flash-1017",
  "enabled_models": ["polza/*", "routerAI/*"]
}
```

This selects `routerAI` and its exact `deepseek/deepseek-v4.1-flash-1017`
model, regardless of the order of the two enabled-model patterns, provided
the model is available in that scope.

If the qualified pair does not exist, Pi checks whether the entire value is
a registered bare ID under `default_provider`, or across providers when no
default provider is set. This preserves namespaced IDs such as
`deepseek/deepseek-v4.1-flash-1017`. If both interpretations exist, the
qualified pair wins. Use the full `routerAI/deepseek/...` form to pin the
destination when model IDs contain a provider-like namespace.

An unresolved configured default stops startup with the requested provider
and model in the error. Use `pi --list-models` to inspect registered IDs and
correct the settings, or override them with `--provider`/`--model`.
Explicit CLI choices and restored sessions retain their precedence;
`model_roles.default` (also spelled `modelRoles.default`) outranks the
legacy `default_provider`/`default_model` settings.

An explicit `--provider` selects the declared provider before checking model
credentials. Separately declared extension providers remain distinct even if
their names are built-in aliases. Undeclared aliases still resolve normally.
With `--model` alone, a matching `default_provider` is preferred; if that
provider does not supply the requested model, Pi warns before using another
provider that does.

The same declared-provider lookup applies to extension model switches, RPC
`set_model`, the classic `/model` command, and saved-model restoration. Pi
resolves explicit destinations against the complete runtime catalog before
checking credentials. A provider omitted from the ready-model picker still
owns its name, so its missing credentials cannot redirect an explicit request
to another declared provider with an alias-like name.

Unlisted native model IDs can still use the native provider's routing defaults.
If a separately declared provider owns a built-in alias such as `open-router`
or `gemini`, its missing models do not inherit the built-in route. Register the
requested model under that provider, or select a registered pair shown by
`pi --list-models` (or `/model` in the classic UI). Rejected explicit runtime
switches preserve the current model and session history. Native aliases that
are not separately declared continue to select the registered native provider.

Per-model `retry.fallbackChains` keys use the same complete catalog to identify
their provider. A separately declared provider cannot borrow another provider's
chain merely because their names are built-in aliases, even when one is absent
from the ready-model list. Role keys still take precedence over per-model keys;
native aliases continue to share a chain when they resolve to the same provider.
If an extension selects a different declared provider after a request fails,
recovery leaves that selection in place for the next request.

An active model scope still controls automatic selection. If a valid
configured default or default role is outside that scope, Pi uses the first
allowed scoped model and warns with both destinations. Check
`enabled_models`, `--models`, `disabled_providers`, workspace scope overrides,
and credentials when this happens. Missing defaults never take this fallback.
Without a configured default, the normal scope-order and automatic selection
behavior is unchanged.

A nonempty `enabled_models`, `--models`, or workspace model scope that leaves
no usable allowed models stops automatic startup. Check the patterns,
credentials, and disabled providers with the diagnostic and `pi --list-models`.
Explicit CLI selections and valid restored sessions retain their existing
precedence. An explicitly empty list still clears the inherited scope.

With `--provider` alone, Pi may use that provider's ready default when the
configured model is incompatible, unavailable, or lacks credentials; it
reports the substitution. A nonblank `--api-key` lets Pi retain a matching
configured model that has no stored credentials. Without a provider override
or scope exclusion, Pi keeps the configured destination for credential setup
instead of switching providers.

Editor sessions started through ACP follow the same configured-model and scope
rules. When no explicit, saved, role, configured-model, or scoped choice applies,
ACP uses its available-model list in order, preferring `default_provider` when
that provider is available. This automatic fallback stays within that list.
Permitted substitutions are reported on stderr, and an unresolved configured
model returns an error to the editor.

Startup records a switch between separately declared providers even when they
share the same model ID or their names are built-in aliases. Continuing the
session therefore restores the selected provider. Runtime model switches also
retain those identities in selection, cycling, and session history. Reopening
an ACP session preserves its selected branch's saved model and thinking level
ahead of new launch defaults.

Example:

```json
{
  "default_provider": "anthropic",
  "default_model": "claude-sonnet-4-20250514",
  "default_thinking_level": "medium",
  "enabled_models": ["claude-*", "gpt-*"]
}
```

### Advisor

Assign an advisor model through `modelRoles.advisor` to review substantial completed
turns. The advisor uses its own provider and compact context.

```json
{
  "modelRoles": { "advisor": "provider/model" },
  "advisor": {
    "enabled": true,
    "timeoutSecs": 15,
    "continueSeverity": "concern"
  }
}
```

- `advisor.enabled` defaults to `true` when an advisor model is configured.
- `advisor.timeout_secs` (alias `timeoutSecs`) defaults to `15` seconds per review.
  A configured main-turn time limit also bounds the review's remaining time.
- `advisor.continue_severity` (alias `continueSeverity`) is the lowest verdict
  that automatically resumes a completed main-agent turn. The default is
  `concern`, matching OMP 18.12.1. Use `blocker` to continue only for blockers,
  or `nit` to include notes; `note` is accepted as an alias for `nit`.

A qualifying verdict reaches the main agent through its ordinary steering queue,
and the original request returns the resulting corrected answer. Completed tools
and the authored prompt are retained in history. Lower-severity notes remain
queued for a later turn.

Both native interactive interfaces display each note as soon as it is recorded
and any required session save succeeds, including notes held below the continuation
threshold. Delivering the queued note into a later model request does not display
it again.

Each authored request permits at most **two automatic advisor continuations**.
The reservation and advisor note are saved before the additional provider request,
so retrying or reopening the same session cannot reset the limit. A new authored
prompt starts a fresh budget. Further notes remain queued when the limit is reached.

Advisor reviews and automatic continuations use the main turn's remaining
wall-clock allowance; a continuation does not grant another full time allowance.
`max_tool_iterations` remains the native limit for each core pass, including
each of the at most two advisor continuations. Reaching that
limit, a length limit, an error, cancellation, or a plan pause does not restart the
main agent. Extension-generated text and custom messages do not renew the advisor
continuation budget.

Automatic continuation respects cancellation, the remaining time budget, paused
queue lanes, and plan review. Advisor provider failures are isolated from the main
turn; a session save failure retains the existing provider-admission fence.
Advisor settings merge field by field across global and project configuration.

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

- `double_escape_action` (string): what Esc Esc does on an idle, empty editor.
  `rewind` (pick an earlier message, rewind to just before it and put it back
  in the editor; the old path stays as a branch, like `/branch`), `fork` (pick
  a message to fork a new session from), `tree`, or `none`. The default
  interactive UI defaults to `rewind`, as OMP does; `--classic` defaults to
  `tree` and has no `rewind`. Alias: `doubleEscapeAction`.
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

- `retry.enabled`: `true` (master switch for automatic recovery)
- `retry.model_fallback`: `true` (allow recovery to switch models)
- `retry.fallback_on`: `"any"` (which eligible failures may switch models)
- `retry.max_retries`: `3`
- `retry.base_delay_ms`: `2000`
- `retry.max_delay_ms`: `60000`

```json
{
  "retry": {
    "enabled": true,
    "model_fallback": true,
    "fallback_on": "any",
    "max_retries": 3,
    "base_delay_ms": 2000,
    "max_delay_ms": 60000
  }
}
```

Set `model_fallback` to `false` to retain same-model retries while preventing
automatic model changes. A configured `fallbackChains` entry is still required
before any model can be selected as a fallback.

`fallback_on` accepts three values:

| Value | Eligible failures that may switch models |
|---|---|
| `any` | All failures admitted by the recovery classifier |
| `usage-limit` | Account quota, subscription window, or credit exhaustion |
| `except-usage-limit` | Transient failures other than account usage exhaustion |

Explicit per-minute, per-second, and concurrency throttles are distinguished
from account exhaustion, including Google quota ids such as
`GenerateRequestsPerMinutePerProjectPerModel`, unless the same error also
names a per-day quota. A bare HTTP 402 or 429, whose body is empty or only
repeats the status, is treated as a usage limit. These filters do not permit recovery from
authentication failures, context overflow, cancellation, or session-persistence
failures, and they do not change the same-model retry budget.

Camel-case aliases `modelFallback` and `fallbackOn` are accepted. The default
interactive `/settings` panel can change both preferences live and persist them.
Editing them while automatic recovery is off keeps recovery off.

#### Credential pools

A comma-separated plural API-key variable, such as `OPENAI_API_KEYS`, enables
rotation when it contains at least two distinct keys. The auth-file path supplies
a stable initial preference. On an admitted rate-limit retry, print, RPC, SDK,
and both terminal interfaces can select a healthy sibling credential after the
failed response has been removed durably. An explicit `--api-key`, SDK key
override, or applicable `FailoverOptions.cli_api_key` remains pinned.

Each credential has its own cooldown: repeated rate limits increase it from
15 seconds through 30, 60, 120, and 240 seconds, capped at 300 seconds. Expiry
permits another attempt; a successful request resets the failure history. A
request started before a newer rate limit cannot clear that newer cooldown.
Changing the configured list removes revoked keys and adds new keys while
retaining cooldowns for keys that remain. Provider aliases share this state.

When every key is cooling, Pi stops using that provider immediately. It can
continue on an eligible configured fallback without spending the remaining
same-provider retries. Another model using the same exhausted pool is skipped;
it does not consume a fallback slot. If no eligible fallback is available, the
request ends with `[CREDENTIAL_POOL_EXHAUSTED]` and the time until the first key
becomes usable. Pi does not wait indefinitely or fall back to a singular
environment key, stored API key, or cached catalog credential to bypass the pool.

The last failed credential is recorded even when retries are disabled or the
retry budget has ended. A new prompt submitted while the pool is exhausted is
refused before its input is appended, so it can be submitted again after the
cooldown or after selecting another provider. Explicit key overrides and stored
OAuth/Bearer credentials retain their normal precedence. Background summaries,
extension completions, and other model-backed helpers also check their managed
credentials when dispatching a request.

### Search

- `search_backend` (string): Backend for the `grep`/`find` tools. `inproc`
  (default) searches in-process with the same engines ripgrep uses (no `rg`
  or `fd` binaries required); `external` shells out to `rg`/`fd` as before —
  kept as a debugging escape hatch. Alias: `searchBackend`.

### Imported rules (foreign formats)

- `foreign_rules` (bool): Import rules from other tools' native config files
  found in the workspace — Cursor `.cursor/rules/*.mdc` and `.cursorrules`,
  Cline `.clinerules`, Copilot `.github/copilot-instructions.md` and
  `*.instructions.md` (`applyTo` globs), Windsurf `.windsurfrules` /
  `.windsurf/rules/`, and `GEMINI.md`. Always-apply rules join the system
  prompt with provenance headers; glob-scoped rules are delivered the first
  time a tool call touches a matching path. Import is read-only, deduped
  against `AGENTS.md`/`CLAUDE.md` (native wins), and budget-capped with a
  truncation notice. Default `true`. Alias: `foreignRules`.

### Ask tool

- `ask_policy` (string): How the `ask` tool (opt-in via `--tools ...,ask`)
  resolves in sessions without an interactive picker (print/JSON mode).
  `recommended` (default) auto-answers with each question's recommended
  option and loudly annotates the result; `error` fails the tool call.
  Alias: `askPolicy`.

### Shell

- `shell_path` (string): Shell binary path. Default `/bin/bash`.
- `shell_command_prefix` (string): Default `set -e`.
- `gh_path` (string): Override path to `gh` for `/share`. Alias: `ghPath`.
- `bash.persistent` (bool): Retain native Bash variables, arrays, functions,
  aliases, options, umask, and working directory between foreground calls on
  Unix. Default `true`; explicit background jobs remain isolated. State is
  saved after normal completion, including an ordinary nonzero result.
  `exit`, shell abort, timeout, and cancellation retain the previous state.
  Overlapping calls use temporary shells and report that their changes will
  not persist. See [Bash shell state](tools/bash.md#persistent-shell-state).
- `bash.auto_background.enabled` (bool): Automatically promote eligible
  non-PTY Bash commands to managed background jobs. Default `true`.
  Alias for the object: `autoBackground`.
- `bash.auto_background.threshold_ms` (integer): Foreground wait before
  promotion, in milliseconds. Default `60000`; values above `600000` are
  clamped. `0` promotes immediately. Alias: `thresholdMs`.

```json
{
  "shell_path": "/bin/bash",
  "shell_command_prefix": "set -e",
  "bash": {
    "persistent": true,
    "autoBackground": {
      "enabled": true,
      "thresholdMs": 60000
    }
  }
}
```

Promotion keeps the same process and execution deadline. A call whose timeout
expires at or before the threshold uses the direct foreground runner. Explicit
`background: true` starts immediately; `background: false` keeps the direct
foreground runner for that call. Automatic promotion is disabled for PTY calls.

Automatic mode uses managed-job storage, including a **16 MiB artifact cap**
even when the command finishes before promotion. Set `enabled` to `false`, or
pass `background: false`, to retain the direct runner's larger spill limit.
Project settings merge the enabled flag and threshold independently, so setting
one does not discard the other. See [Bash execution and background jobs](tools/bash.md)
for timeouts, cancellation, output limits, and examples.

### Project memory

`memory.backend` is `"off"` by default. Set it to `"local"` to enable the
project-scoped SQLite memory bank, its `retain`, `recall`, `reflect`,
`memory_edit`, and `learn` tools, and a budget-capped startup memory block.

```json
{
  "memory": { "backend": "local" },
  "secrets": { "extra_patterns": ["ACME-[A-Za-z0-9]{24}"] }
}
```

Set it to `"cass"` to keep those same local features and also search the
project's historical agent sessions during `recall`:

```json
{
  "memory": { "backend": "cass" }
}
```

CASS history is optional and currently supported on Unix. It uses the
installed `cass` executable with lexical search and index maintenance
disabled. A recent CASS version supporting `--no-maintenance` is required.
Pi does not install CASS or build, repair, or refresh its index. Missing,
incompatible, busy, or unavailable archives leave local memory usable;
the recall result distinguishes unavailable history from an empty search.

History search stays within the canonical project workspace and adds up to
20 excerpts with source paths, line numbers, agent names, and provenance.
These appear separately from durable memory IDs in the tool's `history`
details and use `[cass:N]` citations in its text. Archive hits are not
automatically retained or injected at startup. `/memory` continues to show
the local bank, and `scope: "session"` reads never query CASS.
For project recall, `includeHistory: false` requests only the local bank.
This option and exact session reads remain available in plan mode; history
search requires the usual process permission.

Pi screens complete archived messages before making 2 KiB excerpts. It
rejects partial archive content, limits combined subprocess output to
256 KiB and accepted history text to 64 KiB, and uses a five-second process
deadline. Queries over 4 KiB, excessive output, and timeouts produce a
history status while preserving local results. Cancellation aborts recall.

Project facts, tags, edited memories, and learned skills use the shared
credential detector, including `secrets.extra_patterns`. Detected values
are replaced with irreversible `[REDACTED_*]` markers before storage and
before a learned skill's filename or description is derived. This memory
privacy floor applies even when `secrets.mode` is `"off"`. Reads also screen
older records before recall or startup injection; existing database rows
are not rewritten by reading them.

Shared `scope: "session"` keys retain their separate exact-text contract.
They remain outside project recall, reflection, and startup memory.

### Subagents

- `subagent_structured_results` (bool): Default `false`. Alias: `subagentStructuredResults`.
  When `true`, the `subagent` tool appends a machine-readable
  `<subagent-structured-result>` block to its result text: a compact JSON array
  with one entry per child (`agent`, `step`, `status`, `exitCode`, `output`,
  `error` — the same field names as the `pi.subagent.result.v1` details
  schema). `output`/`error` are truncated to 2 KiB each and the whole block is
  capped at 16 KiB; when entries must be dropped, the final array element is
  `{"truncated": true, "omittedResults": N}`. Default `false` keeps the tool
  output byte-identical to previous releases.

```json
{
  "subagent_structured_results": true
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

- `terminal.show_images` (bool): Default `true`. When `false`, Pi hides image blocks in terminal tool output (images are still stored in sessions/exports).
- `terminal.clear_on_shrink` (bool): Default `false`. When `true`, Pi purges scrollback on terminal shrink to avoid stale rows reappearing after resize.

### Thinking budgets (tokens)

- `thinking_budgets.minimal`: default `1024`
- `thinking_budgets.low`: default `2048`
- `thinking_budgets.medium`: default `8192`
- `thinking_budgets.high`: default `16384`
- `thinking_budgets.xhigh`: default `32768`
- `thinking_budgets.max`: default `65536`

### Packages and resources

- `packages` (array): package sources (string or `{ source, local, kind }`).
- `extensions`, `skills`, `prompts`, `themes` (arrays): resource filters.
- `enable_skill_commands` (bool): default `true`.

### Media (multimodal perception & speech)

- `media.enable_media` (bool): Default `false`. When `true`, activates `inspect_image`, `generate_image`, and `tts`.
- `media.tts_provider` (string): Default `"elevenlabs"`. Supported: `"openai"`, `"elevenlabs"`, `"system"`.
- `media.tts_voice` (string): Voice identifier (e.g. `"alloy"`, `"rachel"`).
- `media.image_gen_provider` (string): Default `"gemini"`. Supported: `"gemini"`, `"dall-e-3"`.
- `media.output_dir` (string): Destination folder for generated media artifacts.
- `media.enable_read_media` / `media.enableReadMedia` (bool): Default `false`. When `true`, activates `read_media`, which attaches a local video/audio file (mp4, webm, mov, mp3, wav, m4a, ogg, flac) as a native media block. Gemini-family models consume audio/video natively. OpenAI Chat Completions models declaring audio input and separate audio pricing consume WAV/MP3 natively and return text; `openai/gpt-audio-1.5` includes that configuration. Unsupported media produces `[media omitted: <name>, <mime>, <size>]`. See [native audio model configuration](models.md#native-audio-with-openai-chat-completions).
- `media.max_bytes` / `media.maxBytes` (integer): Default `5242880` (5 MiB). Hard cap on one `read_media` file in bytes; larger files are rejected with an error naming the cap. Session media can be stored in sidecars and is restored for subsequent provider requests until compaction, so keep this small. Provider-specific per-request transport bounds also apply.

### Computer (desktop automation)

- `computer.enable_computer` (bool): Default `false`. When `true`, activates `computer` tool.
- `computer.require_approval` (bool): Default `true`. Requires operator approval for mouse/keyboard inputs.
- `computer.screenshot_dir` (string): Destination directory for captured screen artifacts.

Native backends:

- **macOS**: a bundled Swift helper run with `/usr/bin/swift` (Xcode Command Line Tools). Screen Recording permission is needed for screenshots and for actions that list or select windows; Accessibility permission is needed for mouse, keyboard, scroll, window focus and `ax_tree`. Grant both to the terminal or application that runs pi.
- **Linux on X11**: `xrandr`, `wmctrl`, `xprop`, `scrot`, `xdotool` and `xclip` on `PATH`, plus `python3` with PyGObject and AT-SPI 2 (`gi.repository.Atspi`) for `ax_tree`. `DISPLAY` must be set. A Wayland session is refused rather than driven through XWayland.
- **Windows**: not implemented yet; every action fails with an error.

### Browser (headless Chromium automation)

- `browser.enable_browser` (bool): Default `false`. When `true`, activates `browser` tool.
- `browser.headless` (bool): Default `true`. Runs browser in headless mode.
- `browser.remote_debugging_port` (integer): CDP debugging port (default `9222`).
- `browser.domain_allowlist` (array of strings): Restricts browser navigation to approved hosts.

### Web Remote & Collab

- `web.port` (integer): Port for `pi web` WebSocket server (default `8080`).
- `web.bind_mode` (string): `"loopback"`, `"tailscale"`, `"lan"`.
- `web.view_only` (bool): Restricts all remote clients to view-only mode.
- `web.max_viewers` (integer): Max concurrent viewer connections (default `4`).

## Full reference

`src/config.rs` is the authoritative list of supported fields and defaulting behavior.
