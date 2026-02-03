# Extension Capture Scenario Suite (bd-2qd)

This document defines a **scenario specification** for the frozen extension sample set in `docs/extension-sample.json`.
It is intended to drive capture + conformance harness work (legacy `pi-mono` → Rust `pi_agent_rust`) with **deterministic**, **auditable** expectations.

Artifacts for the sample set are vendored under `tests/ext_conformance/artifacts/<id>/` (copied from the pinned legacy snapshot; see `docs/EXTENSION_SAMPLE.md`).

---

## Goals

- For each sampled extension:
  - enumerate supported feature categories (tools / slash commands / event hooks / input transforms / UI integration / providers / flags)
  - define **at least one** deterministic scenario per supported category
  - document required configuration/secrets (use **mocked** or **no-env** paths wherever possible)

This spec is intentionally **implementation-agnostic**: it describes what to run and what to assert, not how the harness is implemented.

---

## Conventions

### Scenario IDs

All scenarios have stable IDs:

```
<extension-id>/<category>/<name>
```

Examples:
- `hello/tool/basic`
- `permission-gate/event_hook/dangerous_bash_blocked_no_ui`

### Modes

- **interactive**: `ctx.hasUI = true` and UI calls are scriptable (select/confirm/custom/key input)
- **headless**: `ctx.hasUI = false` (print / json / rpc / non-tty runner)

### Determinism rules

Prefer scenarios that do **not** require:
- network calls
- real OAuth logins
- external binaries not guaranteed in CI

For network/auth-heavy extensions, define:
- an **offline** scenario that asserts the correct error/diagnostic path, and/or
- a **VCR-backed** scenario (recorded HTTP, with secret redaction) when that becomes available.

### Standard test workspace (recommended)

Unless the scenario says otherwise:
- run inside a per-scenario temp directory (unique, deterministic name derived from scenario_id)
- set `TZ=UTC`
- avoid asserting exact timestamps; assert presence/shape
- for git scenarios: init a new repo inside the temp dir

---

## Sample set

IDs (16):

- `permission-gate`
- `protected-paths`
- `todo`
- `hello`
- `antigravity-image-gen`
- `plan-mode`
- `status-line`
- `doom-overlay`
- `sandbox`
- `inline-bash`
- `dynamic-resources`
- `custom-provider-anthropic`
- `custom-provider-qwen-cli`
- `with-deps`
- `subagent`
- `git-checkpoint`

---

## Scenarios

### permission-gate

**Source:** `tests/ext_conformance/artifacts/permission-gate/permission-gate.ts`  
**Feature categories:** event_hook (tool_call), UI integration (select)  
**Notes:** Blocks dangerous `bash` commands (matches `rm -r*`, `sudo`, `chmod/chown 777`).

Scenarios:

- `permission-gate/event_hook/dangerous_bash_blocked_no_ui` (headless)
  - Setup:
    - run with extension enabled
    - ensure `ctx.hasUI = false`
  - Steps:
    - trigger a `bash` tool call with a command matching the dangerous patterns (use a temp-local path), e.g. `rm -rf ./tmp-to-delete`
  - Expected:
    - tool call is blocked
    - reason contains `Dangerous command blocked (no UI for confirmation)`

- `permission-gate/ui_integration/dangerous_bash_prompt_denied` (interactive)
  - Setup: scripted UI choice returns `"No"`
  - Steps: same dangerous `bash` tool call
  - Expected:
    - blocked
    - reason contains `Blocked by user`

- `permission-gate/ui_integration/dangerous_bash_prompt_allowed` (interactive)
  - Setup:
    - scripted UI choice returns `"Yes"`
    - working dir contains `./tmp-to-delete` created by setup
  - Steps: `bash` tool call uses `rm -rf ./tmp-to-delete`
  - Expected:
    - tool call is not blocked by the extension
    - command executes with exit code `0` (or equivalent success signal)
    - `./tmp-to-delete` no longer exists

---

### protected-paths

**Source:** `tests/ext_conformance/artifacts/protected-paths/protected-paths.ts`  
**Feature categories:** event_hook (tool_call), UI integration (notify)  
**Notes:** Blocks `write` and `edit` to paths containing `.env`, `.git/`, `node_modules/`.

Scenarios:

- `protected-paths/event_hook/allow_safe_write` (headless)
  - Steps: tool call `write` to `notes.txt`
  - Expected: not blocked

- `protected-paths/event_hook/block_env_write` (headless)
  - Steps: tool call `write` to `.env`
  - Expected:
    - blocked
    - reason contains `Path ".env" is protected`

- `protected-paths/ui_integration/notify_on_block` (interactive)
  - Steps: tool call `edit` to `node_modules/x.txt`
  - Expected:
    - blocked with reason containing `is protected`
    - UI receives a warning notification containing `Blocked write to protected path:`

---

### hello

**Source:** `tests/ext_conformance/artifacts/hello/hello.ts`  
**Feature categories:** tool  
**Tools:** `hello(name: string)`

Scenarios:

- `hello/tool/basic` (headless)
  - Steps:
    - invoke tool `hello` with `{ "name": "World" }`
  - Expected:
    - tool result text contains `Hello, World!`
    - tool result details contain `{ "greeted": "World" }`

- `hello/tool/param_validation` (headless)
  - Steps: invoke tool `hello` with missing `name`
  - Expected: validation error surfaced (schema/type error), stable error category

---

### todo

**Source:** `tests/ext_conformance/artifacts/todo/todo.ts`  
**Feature categories:** tool, slash_command, event_hook (session_*), UI integration (custom UI)  
**Tools:** `todo(action, text?, id?)`  
**Commands:** `/todos`

Scenarios:

- `todo/tool/smoke_add_list_toggle_clear` (headless)
  - Steps:
    1. `todo { action: "add", text: "first" }`
    2. `todo { action: "add", text: "second" }`
    3. `todo { action: "list" }`
    4. `todo { action: "toggle", id: 1 }`
    5. `todo { action: "clear" }`
  - Expected:
    - add returns `Added todo #<n>: <text>` and updates details.todos/nextId
    - list returns lines like `[ ] #1: first`
    - toggle returns `Todo #1 completed` (or `uncompleted`)
    - clear returns `Cleared <n> todos` and resets `nextId = 1`

- `todo/tool/error_missing_text` (headless)
  - Steps: `todo { action: "add" }`
  - Expected:
    - tool text contains `Error: text required for add`
    - details.error contains `text required`

- `todo/tool/error_missing_id` (headless)
  - Steps: `todo { action: "toggle" }`
  - Expected:
    - tool text contains `Error: id required for toggle`
    - details.error contains `id required`

- `todo/slash_command/requires_ui` (headless)
  - Steps: execute `/todos`
  - Expected: error notification includes `/todos requires interactive mode`

- `todo/ui_integration/render_list_and_close` (interactive)
  - Setup:
    - have at least 1 todo already created (via tool calls)
    - scripted keypress `Escape` to close UI
  - Steps: execute `/todos`
  - Expected:
    - UI overlay renders a list containing `Todos` and `#<id>`
    - UI closes on Escape

- `todo/event_hook/session_fork_reconstructs_state` (interactive)
  - Setup:
    - create 2 todos on the main branch
    - fork session at an earlier entry (pre-second-todo)
  - Steps:
    - in forked branch, run `/todos`
  - Expected:
    - list reflects the branch history (only todos created before fork point)

---

### inline-bash

**Source:** `tests/ext_conformance/artifacts/inline-bash/inline-bash.ts`  
**Feature categories:** input_transform, event_hook (input), UI integration (notify)

Scenarios:

- `inline-bash/input_transform/expand_echo` (headless)
  - Steps:
    - user input: `Value is !{echo 123}`
  - Expected:
    - input is transformed to contain `Value is 123`
    - no extra tool calls are required (extension runs exec internally)

- `inline-bash/input_transform/preserve_bang_command` (headless)
  - Steps: user input: `!echo 123`
  - Expected: action is `continue` (no inline expansion)

- `inline-bash/input_transform/error_substitution` (headless)
  - Steps: user input: `Bad is !{false}`
  - Expected: transformed text includes `[error:` OR includes an exit-code note (implementation-defined), but stable indicator of failure is present

- `inline-bash/ui_integration/notify_expansions` (interactive)
  - Setup: `ctx.hasUI = true`
  - Steps: user input with two expansions
  - Expected: UI info notification includes `Expanded 2 inline command(s):`

---

### plan-mode

**Source:** `tests/ext_conformance/artifacts/plan-mode/index.ts`  
**Feature categories:** slash_command, flags, event_hook (tool_call/context/before_agent_start/turn_end/agent_end), UI integration  
**Commands:** `/plan`, `/todos`  
**Flag:** `--plan`  
**Behavior:** toggles active tool set; blocks non-allowlisted `bash` commands in plan mode; tracks `[DONE:n]`.

Scenarios:

- `plan-mode/slash_command/toggle_plan_mode` (interactive)
  - Steps:
    1. execute `/plan` (enable)
    2. execute `/plan` (disable)
  - Expected:
    - UI notifications include `Plan mode enabled` then `Plan mode disabled`
    - status/widget updated accordingly

- `plan-mode/event_hook/block_destructive_bash` (interactive)
  - Setup: plan mode enabled
  - Steps: assistant attempts a `bash` tool call with `rm -rf ./x`
  - Expected:
    - blocked with reason containing `Plan mode: command blocked (not allowlisted)`

- `plan-mode/slash_command/todos_empty` (interactive)
  - Steps: run `/todos` with no plan extracted yet
  - Expected: UI notify contains `No todos. Create a plan first with /plan`

- `plan-mode/event_hook/extract_plan_and_track_done` (interactive)
  - Setup: plan mode enabled, UI available
  - Steps:
    1. assistant outputs:
       ```
       Plan:
       1. Do thing A
       2. Do thing B
       ```
    2. user selects the execution path (implementation-defined UI)
    3. assistant later responds with `[DONE:1]`
  - Expected:
    - plan steps extracted into todo list
    - status/widget shows completion `1/2`

---

### status-line

**Source:** `tests/ext_conformance/artifacts/status-line/status-line.ts`  
**Feature categories:** event_hook (session_start/turn_start/turn_end/session_switch), UI integration (setStatus)

Scenarios:

- `status-line/ui_integration/turn_progress_status` (interactive)
  - Steps:
    1. start a new session
    2. send one prompt that completes without tools
  - Expected:
    - status key `status-demo` set to `Ready` on session_start
    - updated on turn_start to include `Turn 1...`
    - updated on turn_end to include `Turn 1 complete`

---

### git-checkpoint

**Source:** `tests/ext_conformance/artifacts/git-checkpoint/git-checkpoint.ts`  
**Feature categories:** event_hook (turn_start/session_before_fork/tool_result/agent_end), UI integration (select/notify), external process (git)

Scenarios:

- `git-checkpoint/event_hook/creates_stash_refs` (headless)
  - Setup:
    - init a git repo in temp dir
    - create at least one commit
  - Steps: run a single agent turn that triggers `turn_start`
  - Expected:
    - extension calls `git stash create` (observable via exec logs or stubbed exec)
    - internal checkpoint map updated (assert via extension-visible diagnostics if available)

- `git-checkpoint/ui_integration/restore_on_fork_yes` (interactive)
  - Setup:
    - repo has uncommitted changes
    - scripted UI selection chooses `Yes, restore code to that point`
  - Steps:
    1. run one turn to record a checkpoint
    2. fork session at that entry (trigger `session_before_fork`)
  - Expected:
    - `git stash apply <ref>` executed
    - UI notify contains `Code restored to checkpoint`

- `git-checkpoint/ui_integration/restore_on_fork_no` (interactive)
  - Setup: scripted choice selects `No, keep current code`
  - Expected: no `stash apply` executed

---

### dynamic-resources

**Source:** `tests/ext_conformance/artifacts/dynamic-resources/index.ts`  
**Feature categories:** event_hook (resources_discover)

Scenarios:

- `dynamic-resources/event_hook/returns_paths` (headless)
  - Steps: trigger `resources_discover`
  - Expected:
    - returned payload contains:
      - `skillPaths` with `SKILL.md`
      - `promptPaths` with `dynamic.md`
      - `themePaths` with `dynamic.json`

- `dynamic-resources/harness/resources_loaded` (headless)
  - Setup: run resource loader with this extension enabled
  - Expected:
    - skill list includes the dynamic skill from the extension
    - prompt templates include `dynamic.md`
    - theme list includes the dynamic theme JSON

---

### with-deps

**Source:** `tests/ext_conformance/artifacts/with-deps/index.ts`  
**Feature categories:** tool  
**Notes:** Requires npm dependencies present in extension dir for real runtime. Conformance should validate dependency resolution.

Scenarios:

- `with-deps/tool/parse_duration_valid` (headless)
  - Steps: invoke `parse_duration { duration: "1h" }`
  - Expected: text contains `1h = 3600000 milliseconds`

- `with-deps/tool/parse_duration_invalid` (headless)
  - Steps: invoke `parse_duration { duration: "not-a-duration" }`
  - Expected:
    - `isError = true`
    - text contains `Invalid duration:`

---

### subagent

**Source:** `tests/ext_conformance/artifacts/subagent/index.ts`  
**Feature categories:** tool, UI integration (confirm), external process (spawns `pi` subprocess), filesystem (temp prompt files)
**Tool:** `subagent(...)`

Scenarios (deterministic-first):

- `subagent/tool/invalid_params_mode_count` (headless)
  - Steps: call `subagent` with both `agent+task` and `tasks` present
  - Expected:
    - tool result text contains `Invalid parameters. Provide exactly one mode.`
    - tool result details include `{ mode: "single" | ... }` with empty results

- `subagent/tool/unknown_agent` (headless)
  - Setup: ensure no agents with the requested name exist
  - Steps: `subagent { agent: "does-not-exist", task: "hi" }`
  - Expected:
    - result contains `Unknown agent: does-not-exist`
    - `exitCode = 1` in details

- `subagent/ui_integration/deny_project_agents` (interactive)
  - Setup:
    - have a project agent in `.pi/agents/`
    - call subagent with `agentScope: "project"` and `confirmProjectAgents: true`
    - scripted confirm returns `false`
  - Expected:
    - tool result text contains `Canceled: project-local agents not approved.`

VCR-backed (future):

- `subagent/tool/single_smoke` (headless, VCR)
  - Steps: run a simple user-agent (`scout`) that does only `ls/read`
  - Expected: deterministic JSON mode output captured and summarized

---

### antigravity-image-gen

**Source:** `tests/ext_conformance/artifacts/antigravity-image-gen/antigravity-image-gen.ts`  
**Feature categories:** tool, network/auth (OAuth), filesystem (optional save)
**Tool:** `generate_image(prompt, model?, aspectRatio?, save?, saveDir?)`

Deterministic scenarios:

- `antigravity-image-gen/tool/missing_credentials` (headless)
  - Setup: no Google Antigravity credentials present (no `/login`, no stored key)
  - Steps: invoke `generate_image { prompt: "a cat" }`
  - Expected: stable error message containing `Missing Google Antigravity OAuth credentials`

- `antigravity-image-gen/tool/save_mode_custom_without_dir` (headless)
  - Steps: invoke `generate_image { prompt: "a cat", save: "custom" }` with no `PI_IMAGE_SAVE_DIR`
  - Expected: deterministic error or saveError surfaced (exact message may differ; assert presence of `save`/`custom` and missing dir indicator)

VCR-backed (future):

- `antigravity-image-gen/tool/vcr_generate_image` (headless, VCR)
  - Setup: recorded HTTP stream response with an embedded base64 image; secrets redacted
  - Expected:
    - tool result includes an `image` content block with mimeType
    - summary text contains provider/model + aspectRatio

---

### doom-overlay

**Source:** `tests/ext_conformance/artifacts/doom-overlay/index.ts`  
**Feature categories:** slash_command, UI integration (overlay), network/filesystem (auto-download WAD)
**Command:** `/doom-overlay`

Deterministic scenarios:

- `doom-overlay/slash_command/requires_ui` (headless)
  - Steps: execute `/doom-overlay`
  - Expected: UI notify error contains `DOOM requires interactive mode`

- `doom-overlay/slash_command/wad_download_failure` (interactive, offline)
  - Setup: no network available (or WAD URL blocked)
  - Steps: execute `/doom-overlay`
  - Expected: error notification contains `Failed to download DOOM WAD file`

---

### sandbox

**Source:** `tests/ext_conformance/artifacts/sandbox/index.ts`  
**Feature categories:** tool override (bash), flags, slash_command, event_hook (session_start/user_bash/session_shutdown), UI integration (notify/setStatus)
**Flag:** `--no-sandbox`  
**Command:** `/sandbox`

Deterministic scenarios (no external sandbox runtime required):

- `sandbox/flags/no_sandbox_disables` (interactive)
  - Setup: run with `--no-sandbox`
  - Steps: start session
  - Expected: warning notification contains `Sandbox disabled via --no-sandbox`

- `sandbox/slash_command/sandbox_when_disabled` (interactive)
  - Setup: sandbox disabled (via flag or config)
  - Steps: execute `/sandbox`
  - Expected: notify contains `Sandbox is disabled`

Best-effort (environment-dependent):

- `sandbox/tool/bash_still_works_without_sandbox` (headless)
  - Setup: sandbox disabled
  - Steps: run bash tool `echo ok`
  - Expected: output contains `ok`

VCR/CI-specific (future):

- `sandbox/event_hook/initializes_and_sets_status` (interactive, requires bubblewrap/sandbox runtime)
  - Expected: status includes `🔒 Sandbox:` and notify contains `Sandbox initialized`

---

### custom-provider-anthropic

**Source:** `tests/ext_conformance/artifacts/custom-provider-anthropic/index.ts`  
**Feature categories:** provider registration, OAuth (`/login`), streaming implementation (`streamSimple`)
**Provider:** `custom-anthropic`  
**Env key:** `CUSTOM_ANTHROPIC_API_KEY`  
**API id:** `custom-anthropic-api`

Scenarios:

- `custom-provider-anthropic/provider/models_listed` (headless)
  - Steps: list models
  - Expected: includes:
    - `custom-anthropic/claude-opus-4-5`
    - `custom-anthropic/claude-sonnet-4-5`

- `custom-provider-anthropic/provider/missing_api_key_errors` (headless)
  - Setup: no `CUSTOM_ANTHROPIC_API_KEY`, no oauth credentials
  - Steps: attempt to stream a minimal prompt with provider `custom-anthropic`
  - Expected: deterministic error about missing API key/credentials

VCR-backed (future):

- `custom-provider-anthropic/provider/vcr_streaming_smoke` (headless, VCR)
  - Setup: recorded streaming response (SSE), secrets redacted
  - Expected:
    - emits text deltas
    - tool calls round-trip correctly (if tools enabled)

---

### custom-provider-qwen-cli

**Source:** `tests/ext_conformance/artifacts/custom-provider-qwen-cli/index.ts`  
**Feature categories:** provider registration, OAuth device flow (`/login`), OpenAI-compatible API
**Provider:** `qwen-cli`  
**Env key:** `QWEN_CLI_API_KEY`  
**API id:** `openai-completions`

Scenarios:

- `custom-provider-qwen-cli/provider/models_listed` (headless)
  - Steps: list models
  - Expected: includes `qwen-cli/qwen3-coder-plus` and `qwen-cli/qwen3-coder-flash`

- `custom-provider-qwen-cli/provider/missing_api_key_errors` (headless)
  - Setup: no `QWEN_CLI_API_KEY`, no oauth credentials
  - Steps: attempt to stream a prompt with provider `qwen-cli`
  - Expected: deterministic error about missing credentials / API key

VCR-backed (future):

- `custom-provider-qwen-cli/provider/vcr_openai_compat_smoke` (headless, VCR)
  - Expected:
    - request uses OpenAI-compatible payload/headers
    - response parsing yields stable assistant output

---

### plan-mode / sandbox / doom-overlay (UI-heavy extensions)

These are intentionally covered above with **headless error-path** + **interactive scripted UI** scenarios.
The harness should prioritize running the headless paths in CI, and gate the heavier interactive/network paths behind opt-in runners.

---

## Open questions (for harness implementors)

- How do we represent `ctx.hasUI` / UI scripting in the Rust harness in a way that matches legacy behavior?
- For provider/network scenarios, do we standardize on a VCR format (SSE recording) shared across providers and extensions?
- What is the canonical “command execution transcript” format for asserting `pi.exec(...)` behavior deterministically (stdout/stderr/exitCode + timing)?

