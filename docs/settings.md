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

Trusted project settings override global settings on a per-field basis.
An untrusted project's settings are excluded from the merge.

Nested settings such as `compaction`, `retry`, `images`, `terminal`,
`branch_summary`, and `thinking_budgets` also merge field by field. A project
can override one nested value while inheriting the other global values.
Missing values fall back to built-in defaults only when neither scope supplies
them (see `src/config.rs`).

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

- `compaction.reserve_tokens` becomes `8192`.
- `compaction.enabled` remains `false`, inherited from global settings.

The `tools` object merges each activation flag independently, and `loadMode`
merges entries by tool name. An omitted activation flag inherits the global
value; an explicit `false` disables a globally enabled optional tool for that
trusted project. See [Optional tools](#optional-tools).

## ACP workspace settings

For `pi --acp`, creating a session, loading a saved session from disk, or
forking a session resolves global settings together with the requested
workspace's trusted project settings. Untrusted project settings are excluded.
Explicit CLI choices, including `--tools` and `--no-tools`, retain precedence.
A running session keeps the settings admitted when it was created, including
its prompt, steering, and media policy; reattaching does not replace them.

The launch-time `--trust` flag applies to the launch workspace and the contents
approved there. It is not blanket consent for other workspaces requested later
by an editor. Existing persisted trust decisions are checked for the requested
workspace and current contents. Global `trustAllWorkspaces` and explicit
`PI_WORKSPACE_TRUST` policies retain their existing scope.

`PI_CONFIG_PATH` still selects one file instead of the global/project merge.
A relative override is resolved against Pi's launch directory and keeps that
location when a session requests a different workspace. The startup skills
block is omitted for a different or untrusted workspace.

## Magic keywords

Standalone lowercase words in **user-authored prose** can change how Pi handles
a turn. Code spans, fenced or indented code, HTML/XML, paths, identifiers, and
function-call spellings such as `chillax()` do not activate them. Generated
attachment content, extension messages, and BTW injections are not new authored
keyword requests. The original visible user message is preserved.

| Word | Native behavior |
| --- | --- |
| `ultrathink` | Temporarily use the selected model's highest supported thinking tier for this turn. |
| `chillax` | Add a brief-reasoning instruction. On supported adaptive Anthropic models, lower an enabled pinned thinking level to the lowest supported tier and keep that setting until changed. |
| `orchestrate` | Add the parallel delegation and verification instruction for this turn. |
| `workflowz` | Add the deterministic multi-agent workflow instruction for this turn. |
| `jevify` | When `eval` is available, add a bulk-classification instruction using native `judgeBatch()` / `judge_batch()` with bounded batches and explicit coverage/error accounting. |

For example, write `chillax, check this small fix` or `jevify this list of
changes against the agreed rubric`. `jevify` does not enable eval, choose an
unconfigured model, or expand a request or cost budget. Configure the native
`judge_model` or a trusted model role as described by the eval tool.

`chillax` wins over `ultrathink` within the same logical turn, including steering
messages delivered before the next provider request. It never turns thinking on
or raises the pinned effort. Numeric lowering is restricted to the
`anthropic-messages` transport with verified adaptive thinking support; the
catalog's `forceAdaptiveThinking` and disabled thinking tiers are respected.
Unknown or inverted custom effort mappings are not guessed. Other models still
receive the brief-reasoning instruction. Native Pi does not currently have OMP's
automatic thinking classifier.

The lowered effort is recorded in session history and survives resume. A later
explicit thinking selection changes it normally. Hidden instructions remain
turn-local: a follow-up starts a new instruction scope, and the next ordinary
turn does not inherit a previous keyword's notice. Native notices currently
append to the turn's system prompt, so there is no guarantee that injecting a
notice mid-turn preserves a provider's prompt-cache prefix.

All keyword switches default to enabled. Set the following in global or trusted
project settings:

```json
{
  "keywords": {
    "enabled": true,
    "ultrathink": true,
    "chillax": true,
    "orchestrate": true,
    "workflowz": true,
    "jevify": true
  }
}
```

Set `keywords.enabled` to `false` to disable all effects, including custom
keywords, or disable individual words. These fields merge independently across
settings scopes. OMP's `magicKeywords` object name (also `magic_keywords`) and
its `workflow` setting for `workflowz` are accepted aliases; use one spelling
per field in a file. Activations are recorded as `magic_keyword` session entries;
actual pinned reductions also record a thinking-level change.

## Supported settings (snake_case JSON keys)

### Language servers (LSP)

Pi includes definitions for rust-analyzer, TypeScript Language Server, Pyright,
gopls, and clangd. Install the server executable yourself; the defaults select
and launch it lazily for matching source files. A settings entry is only needed
to override a default or add another server.

See [language-server setup and configuration](lsp-configuration.md) for the
installation table, automatic detection behavior, and `lsp.servers` examples.

### Optional tools

Persistently enable `eval`, `github`, `security_scan`, or `subagent` in the
existing `tools` object. All four remain disabled by default.

| Setting | Snake_case spelling | Tool | Default |
| --- | --- | --- | --- |
| `tools.enableEval` | `tools.enable_eval` | `eval` | `false` |
| `tools.enableGithub` | `tools.enable_github` | `github` | `false` |
| `tools.enableSecurityScan` | `tools.enable_security_scan` | `security_scan` | `false` |
| `tools.enableSubagent` | `tools.enable_subagent` | `subagent` | `false` |

Use JSON booleans, not strings such as `"true"`. The two spellings of each
setting are equivalent; use one spelling per field in a file.

For example, add this to `~/.pi/agent/settings.json`:

```json
{
  "tools": {
    "enableEval": true,
    "enableGithub": true,
    "enableSecurityScan": true,
    "enableSubagent": true
  }
}
```

When `--tools` is omitted, Pi adds the enabled optional tools to its existing
default tool list. An explicit `--tools` list selects the tools for that run
and overrides these four flags, including when the list is empty or exactly
matches the default list. For example, `pi --tools read,grep,find,ls` does not
enable any of these four tools, even if all four settings are `true`.
Conversely, `pi --tools read,eval` selects `eval` even when `enableEval` is
`false`.

`--no-tools` and an explicit `--tools ""` disable all built-in tools, including
settings-gated browser, computer, media, and memory tools. A value containing
only whitespace or commas also selects no built-ins. The SDK's explicit empty
`enabled_tools` list and ACP sessions follow the same rule. Extensions and MCP
servers retain their existing loading and trust controls. Plan mode cannot
reintroduce a built-in tool excluded by this selection.

The same flags work in a trusted project's `.pi/settings.json`. For example,
`{"tools":{"enableEval":false}}` disables a global `enableEval: true` while
inheriting the other global tool flags and load modes. Project settings are
applied only after the existing workspace-trust decision; an untrusted project
cannot activate these tools or override global choices. Editing the project
settings invalidates a stored trust decision. `PI_CONFIG_PATH` retains its
existing explicit single-file behavior and skips the global/project merge.

In the default terminal UI, omitting `--tools` keeps the selection driven by
settings across session replacements. Global optional-tool settings are read
again when `/new`, `/resume`, `/fork`, or `/reload` builds a session. An explicit
CLI selection remains authoritative across those operations.

A launch grant from `--trust`, a prompt, or the stored workspace decision
applies to the admitted project configuration and executable resources.
Changing that surface while Pi is running causes replacement and resource
reload to report `PI_WORKSPACE_TRUST_RESTART_REQUIRED`. Restart Pi to
re-establish trust and resolve the project resources together; approving the
new digest in another process does not refresh cached paths in the running
session. Refusal preserves the current session and previously loaded resources.
Global `trustAllWorkspaces` and `PI_WORKSPACE_TRUST=trusted` retain their
documented broad scope, while current global policy and stored trust are
checked again. A workspace launched untrusted remains excluded until restart.
The classic terminal UI also revalidates this admission before `/reload`
resolves packages or refreshes resources; a refusal preserves its loaded
prompts, skills, and current session.

Enabling `subagent` grants access to bundled and global agent definitions.
Project profiles from the nearest `.pi/agents/*.md` directory require workspace
trust independently of that activation setting. They are part of the trust
digest even when discovered in an ancestor directory: a profile can select
child tools and override a bundled or global agent. Untrusted project profiles
are excluded from both `scope: "both"` and `scope: "project"`.
`scope: "user"` remains available without reading project profiles.

`tools.loadMode.<name>` (also `tools.load_mode.<name>`) configures tool exposure
as `essential`, `discoverable`, or `off`. An explicit `off` setting excludes a
tool from implicit selection, including tools automatically added by browser,
computer, media, and memory settings, and the default `manage_skill` tool.
For example, `{"tools":{"enableEval":true,"loadMode":{"eval":"off"}}}` keeps
`eval` disabled when `--tools` is omitted. An explicit `--tools eval` overrides
that exclusion. Explicit names do not enable a separate backend: the memory
bank still requires `memory.backend` to be `"local"` or `"cass"`. The generated
`xdev` dispatcher still requires discoverable tools and is disabled by
`loadMode.xdev: "off"`.

Load-mode values ignore surrounding whitespace and letter case. Any other
string, such as `"disabled"` or `"discoverble"`, produces a startup warning
naming the settings file, dotted entry, and rejected value. The invalid value
is ignored and normal selection/default exposure applies: `"disabled"` does
not turn a tool off. Use `"off"` to exclude it from implicit activation.
Extension and MCP tool names are accepted as map keys. These rules also apply
when tools register after startup, an extension reloads, or an MCP server
refreshes its catalog. Use the published MCP name, such as
`mcp__docs__search`, rather than the server's unqualified `search` name.

New extension and MCP tool names default to `essential`. Replacements preserve
the existing exposure unless configured. Setting one to `discoverable` hides
its schema and makes its current
description and arguments available through `xdev`; setting it to `off`
excludes it from lookup, schemas, and discovery. An extension's
`setActiveTools` cannot reactivate a tool excluded by `off`. For example:

```json
{
  "tools": {
    "loadMode": {
      "my_extension_tool": "discoverable",
      "mcp__docs__search": "discoverable",
      "mcp__docs__delete": "off"
    }
  }
}
```

These settings do not load an extension, connect an MCP server, or grant
permission to execute its tools. The existing admission and authorization
checks still apply. A late discoverable tool can supply the generated `xdev`
dispatcher, including alongside an explicit built-in selection such as
`--tools read`. `--no-tools` and `loadMode.xdev: "off"` suppress that generated
dispatcher; choose `essential` for mounted tools that should remain directly
visible in those cases. Catalog refresh updates discovery together with the
active tools, so shelved or removed tools no longer appear in `xdev list`.

`subagent` remains activatable with `enableSubagent: true`: its built-in
off-by-default tier is not an explicit `loadMode.subagent: "off"` setting.

Enabled `eval`, `github`, and `security_scan` use the `discoverable` tier by
default. Pi lists their names and purposes in its default system prompt, and
`xdev` can describe, run, or promote them. Use `loadMode.<name>: "essential"`
to expose an enabled tool directly in the model's schema. If `xdev` is `off`,
Pi omits the discovery instructions; use `essential` for tools that should
remain directly available. A custom system prompt keeps its existing contract
and does not receive Pi's automatic discovery inventory.

`xdev run` checks approval and extension call hooks for both the dispatcher
and the actual target, using the target's own name and arguments for its
checks. A denial at either layer prevents execution. Existing policies on
`xdev` still apply, so an interactive approval handler can ask at both layers
when approval is required. Disabling either tool during a policy callback
also prevents execution. Discovery does not bypass target-specific controls.

Load modes never activate an optional tool by themselves. These activation
flags use the existing tool registry and preserve existing approval and
workspace-trust requirements.

Native child agents keep their profile's built-in tool list across one-shot
runs, retries, background runs, and persistent workers. Inherited settings
cannot add optional tools or automatic browser, computer, media, memory, or
`manage_skill` access beyond that list. A profile can name those tools
explicitly; backend requirements still apply, including `memory.backend`
for memory tools. The `xdev` dispatcher exposes only admitted discoverable
targets. Extensions and MCP servers retain their separate loading and
authorization controls; this built-in selection is not an operating-system
sandbox. Ordinary operator launches keep their existing automatic-tool
behavior.

### Appearance

- `theme` (string): Theme to apply. Built-ins: `dark`, `light`, `solarized`; also
  accepts a discovered theme name or a path to a theme JSON file. `light/dark`,
  `auto`, or `system` auto-detect dark/light from the terminal background via the
  `COLORFGBG` environment variable (dark when unavailable). If unset, auto-detects
  the same way.
- `hide_thinking_block` (bool): Hide thinking blocks in interactive output. Default `false`.
- `show_hardware_cursor` (bool): Show terminal hardware cursor. Default `false` unless
  `PI_HARDWARE_CURSOR=1`.

### Session titles

Interactive sessions can use the configured `tiny` or `smol` model role to name
the first completed exchange. `titling.auto_title` (also `title.autoTitle`)
defaults to `true`; setting it to `false` disables both naming and refresh.

Set `title.refreshAfterPrompts` to a positive integer to reassess automatically
generated titles as the task changes. The equivalent native spelling is
`titling.refresh_after_prompts`. Refresh is disabled by default (`0`).

```json
{
  "title": {
    "autoTitle": true,
    "refreshAfterPrompts": 3
  }
}
```

After that many substantive operator prompts and a successfully saved reply,
Pi asks the auxiliary model whether the existing title still fits. It requests
a replacement only when the title has drifted. Generated continuation messages
and brief acknowledgments do not advance the interval. Counts follow the saved
conversation branch, including prompts no longer in the model's compacted
context; successful checks survive resume.

Names assigned with `/name` are preserved, including when you choose the same
text as a generated title. Older saved names without automatic-name provenance
are also preserved. A partially loaded large session waits for its complete
history before becoming eligible. Refresh uses bounded, privacy-screened conversation text
and excludes reasoning, tool payloads and media. It stops when its session or
branch changes, and only publishes a replacement after the name is saved.

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

Optional settings can trigger automatic compaction earlier:

- `compaction.threshold_tokens` (alias `thresholdTokens`): A positive `u32`
  integer token count.
- `compaction.threshold_percent` (alias `thresholdPercent`): A finite number
  greater than `0` and at most `100`, expressed as a percentage of the active
  model's context window.

Both default to unset, preserving the existing automatic trigger. Omitting a
field or setting it to `null` supplies no override in that configuration layer;
the usual global/project field merge still applies. Invalid non-null values
produce a configuration error.

For example, keep the existing reserve and keep-recent budgets while adding
earlier triggers:

```json
{
  "compaction": {
    "thresholdTokens": 60000,
    "thresholdPercent": 75
  }
}
```

Either threshold can be used alone. When both are set, automatic compaction
becomes eligible when the estimated context reaches or exceeds the smallest
of:

- The active model's context window minus `reserve_tokens`, with a minimum of
  zero.
- `threshold_tokens`, when set.
- The active model's context window multiplied by `threshold_percent / 100`,
  rounded down to whole tokens, when set.

This preserves the reserved response headroom even if an explicit threshold is
larger. Percentages are recalculated against the active model's context window
when models change. The policy applies to native print, SDK, classic and FTUI
interactive, ACP, and RPC sessions using configuration-derived settings.

`keep_recent_tokens` and the safe-cut rules still apply: reaching a threshold
does not force compaction when no older prefix can be safely summarized.
Explicit `/compact`, `/shake`, and their SDK operations continue to bypass the
automatic thresholds and enable switch.

#### Ordered automatic methods

`compaction.method_order` (alias `methodOrder`) selects the order in which
automatic compaction tries native methods:

```json
{
  "compaction": {
    "methodOrder": ["shake", "soft"],
    "thresholdPercent": 75
  }
}
```

| Method | Behavior |
|--------|----------|
| `shake` | Builds a local checkpoint that preserves conversation text and replaces bulky tool output with references. Makes no model request. |
| `soft` | Requests a model-written summary through the session's existing provider and spending guard. |
| `remote` | Unavailable in the native policy because encrypted remote windows do not yet have a supported replay path. Advances to the next configured method without sending a request. |

The list must contain one to three distinct methods. Unknown methods, empty
lists, and duplicates are configuration errors. A project list replaces the
global list in full. An omitted or `null` list supplies no override; with no
list in any layer, the existing `compactionMode` policy remains in effect.
An explicit list overrides that legacy policy, including its aggressive
keep-recent adjustment, and preserves the configured retention budget.

Each attempt reads the same source snapshot. A candidate is accepted only if
its projected context, including the actual retained tail and preserved rule
reminders, is smaller than the original context and below the automatic trigger.
An unavailable method,
an ordinary provider failure, or insufficient reclamation advances to the
next method. Cancellation, privacy refusals, authentication failures, cost
limits, and persistence failures stop the operation. Provider requests remain
billable even when their candidate is insufficient; fallback does not reset
the session's spending limit.

Only the accepted candidate is installed through the existing session
transaction. The complete candidate is checked again under the installation
lock, using the current reader and any new retained turns; a background result
that no longer fits is discarded before saving. Exhausting the list leaves
the session history unchanged and
reports an error; it does not append an ineffective summary or silently choose
an unconfigured local truncation. Model changes recalculate both the trigger
and retained-tail estimate using the active reader.

Both `compaction.mode: "text"` and `"snapcompact"` work with explicit method
ordering. Snapcompact constructs one privacy-screened archive per operation
and budgets the same reading that the selected model will receive. Vision
models use the archive images; text-only models and sessions with images
blocked use retained source text within the session's source-token allowance.
If image rendering exceeds its archive limits, the text reading is budgeted
instead. A small summary does not hide a large archive: the archive reading,
summary envelope, retained turns, and preserved reminders must fit together.
The stored archive remains independent of the model and is reprojected after
resume or a model switch.
Manual `/compact`, `/shake`, and handoff commands retain their explicit
operation semantics and do not run the automatic method list.

With an explicit method list, `session_before_compact` hooks may cancel
automatic or overflow compaction, but returning a replacement compaction is
rejected without installing it. Replacement summaries can change the retained
cut or rendering outside the configured method policy. Extension replacements
remain supported for ordinary manual compaction and the legacy automatic
policy without a method list.

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

- `tools.enableSubagent` (bool): Default `false`. Persistently enables the native
  `subagent` tool when `--tools` is omitted. See [Optional tools](#optional-tools)
  for aliases, CLI precedence, and workspace-trust behavior.
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

| Setting | camelCase alias | Effect |
| --- | --- | --- |
| `media.enable_media` | `media.enableMedia` | Default activation for `inspect_image`, `generate_image`, and `tts` |
| `media.enable_inspect_image` | `media.enableInspectImage` | Override activation of `inspect_image` |
| `media.enable_generate_image` | `media.enableGenerateImage` | Override activation of `generate_image` |
| `media.enable_tts` | `media.enableTts` | Override activation of `tts` |

All four switches accept JSON booleans. Each individual switch, including an
explicit `false`, takes precedence over the aggregate setting. If neither is
set, the tool remains disabled. `read_media` has its own independent switch.
For example, this enables image inspection and generation without speech:

```json
{
  "media": {
    "enableMedia": true,
    "enableTts": false
  }
}
```

Global and trusted project settings first merge field by field, with project
values overriding the same global field regardless of spelling. Missing and
`null` values inherit. Individual switches then override the merged aggregate,
even when the individual value was inherited: a global `enable_tts: false`
survives a project `enable_media: true`, and a global `enable_tts: true`
survives a project `enable_media: false`. Set the project's individual switch
to change that choice.

Untrusted project settings remain excluded. `--no-tools` and an explicit empty
tool selection still disable built-ins. Explicitly naming a media tool retains
its existing override behavior. Ordinary nonempty `--tools` selections retain
automatic media additions; restricted child-agent and SDK tool allowlists
exclude unlisted tools. Implicit activation respects
`tools.loadMode.<name>: "off"`. Execution retains its existing approval
requirements. See [Optional tools](#optional-tools).

Use one spelling per field in each file; supplying both is a duplicate-field
error. Strings such as `"true"` are invalid. Unknown media settings with non-null values
produce a startup warning naming the file and dotted setting and explaining
that it is ignored. An explicit `RUST_LOG` filter retains control over log
visibility.

- `media.vision_provider` / `media.visionProvider` (string): Default `"gemini"`. Supported: `"openai"`, `"anthropic"`, `"gemini"`.
- `media.vision_model` / `media.visionModel` (string): Default inspection model; otherwise chosen for the selected provider.
- `media.tts_provider` / `media.ttsProvider` (string): Default `"openai"`. Supported: `"openai"`, `"xai"`.
- `media.tts_voice` / `media.ttsVoice` (string): Provider voice identifier; defaults to `"alloy"` for OpenAI or `"eve"` for xAI.
- `media.image_gen_provider` / `media.imageGenProvider` (string): Default `"openai"`. Supported: `"openai"`, `"gemini"`, `"xai"`.
- `media.image_gen_model` / `media.imageGenModel` (string): Default generation model; otherwise chosen for the selected provider.
- `media.enable_read_media` / `media.enableReadMedia` (bool): Default `false`. When `true`, activates `read_media`, which attaches a local video/audio file (mp4, webm, mov, mp3, wav, m4a, ogg, flac) as a native media block. Gemini-family models consume audio/video natively. OpenAI Chat Completions models declaring audio input and separate audio pricing consume WAV/MP3 natively and return text; `openai/gpt-audio-1.5` includes that configuration. Unsupported media produces `[media omitted: <name>, <mime>, <size>]`. See [native audio model configuration](models.md#native-audio-with-openai-chat-completions).
- `media.max_bytes` / `media.maxBytes` (integer): Default `5242880` (5 MiB). Maximum decoded bytes per audio/video attachment, shared by `read_media`, CLI `@file` inputs, and both interactive interfaces. Files exactly at the cap are accepted; larger `@file` inputs produce a notice naming the cap, and `read_media` returns an error. The same limit applies to interactive prompt templates. Image resizing and text-file limits are separate. Session media can be stored in sidecars and is restored for subsequent provider requests until compaction. Provider-specific per-request transport bounds also apply.

For provider, model, and voice choices, explicit tool-call arguments win over
settings, followed by the corresponding `PI_VISION_*`, `PI_IMAGE_GEN_*`, or
`PI_TTS_*` environment fallback and then the built-in default.

`media.output_dir` is not supported. Use the `output_path` argument of
`generate_image` or `tts` to choose a new destination within the workspace.
Without it, artifacts use `images/generated_<id>.<extension>` or
`audio/speech_<id>.<extension>`. Existing files are not overwritten. Activating
a tool does not supply the provider credentials needed to execute it. Native
media tools use API-key credentials from the owning session's `auth.json`,
after any configured provider key pool and singular environment variables.
Only the requested provider's API key is resolved; chat-session OAuth tokens
and chat-only CLI/SDK key overrides are not forwarded. See
[media authentication](tools/media_tools.md#activation-and-credentials).

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
- `browser.headless` (bool): Default `true`. Set `false` to show the browser window Pi launches. `PI_BROWSER_HEADLESS` overrides it. It has no effect when Pi attaches to an existing browser through `PI_BROWSER_CDP_URL`.
- `browser.domain_allowlist` / `browser.domainAllowlist` (array of strings): Restricts browser navigation to approved hosts.

The browser executable and user agent are set with the `PI_BROWSER_EXECUTABLE` and `PI_BROWSER_USER_AGENT` environment variables, not settings. Pi always asks the operating system for a free debugging port, so it has no port setting; to use a fixed port, start the browser yourself and attach with `PI_BROWSER_CDP_URL`. See [browser automation](tools/browser-cdp.md).

### Local browser companion

`pi web` starts the normal FrankenTUI session with supervised browser access on
loopback. Use `--port`, `--view-only`, and `--max-viewers` on that subcommand.
The local terminal must accept each browser prompt and retains tool approval
and login input. There are no persistent `web.*` settings (Pi reports them as
unrecognised). See [the browser companion guide](tools/web_remote.md).

`pi web --terminal` selects the fullscreen terminal mirror, with locally
approved remote control and a separate browser composer. This mode additionally
accepts `--bind tailscale` or `--bind lan`; `--view-only` disables remote input.
Its viewer limit is 1–64 (default 4), while the companion limit is 1–16.
The modes own separate credentials and input permissions and cannot run on the
same launch. Neither mode adds persistent settings or supports headless access.

## Full reference

`src/config.rs` is the authoritative list of supported fields and defaulting behavior.
