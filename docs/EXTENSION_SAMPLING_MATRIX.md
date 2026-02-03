## Stratified Extension Sampling Matrix

This matrix uses **deterministic sampling criteria** from `CONFORMANCE.md` (target sample size **16**, min **12**, max **20**) and the raw candidate pool from `docs/EXTENSION_CANDIDATES.md`. It defines **axes + quotas** and maps **every candidate** to the axes so selection can be mechanical and reproducible.

> **Note:** Tags below are **inferred** from README/descriptions. A static scan should validate and adjust before final selection.

---

## 1) Sampling Axes & Quotas (Target = 16)

### Diversity Tags (must cover if present in pool)
From `CONFORMANCE.md`:

| Tag | Meaning | Minimum Coverage (target=16) |
|---|---|---|
| `tool_only` | tools only; no slash commands, no event hooks | ≥ 2 |
| `slash_command` | registers ≥1 slash command | ≥ 2 |
| `event_hook` | listens to event hooks | ≥ 3 |
| `ui_integration` | uses `pi.ui.*` or UI hooks | ≥ 4 |
| `network_usage` | uses `http` hostcalls | ≥ 2 |
| `filesystem_usage` | uses `read/write/edit` or fs calls | ≥ 4 |

### Runtime Tier

| Tier | Meaning | Minimum Coverage |
|---|---|---|
| `legacy-js` | single JS/TS file extension | ≥ 8 |
| `multi-file` | directory/extension folder | ≥ 4 |
| `pkg-with-deps` | package.json + deps | ≥ 2 |
| `provider-ext` | custom provider extension | ≥ 2 |

### Complexity

| Level | Heuristic | Minimum Coverage |
|---|---|---|
| `small` | ≤ ~2 KB or trivial hooks | ≥ 3 |
| `medium` | multi-hook or non-trivial logic | ≥ 6 |
| `large` | complex UI/game/provider | ≥ 3 |

### I/O Pattern

| Pattern | Minimum Coverage |
|---|---|
| `fs-heavy` | ≥ 3 |
| `network-heavy` | ≥ 2 |
| `ui-centric` | ≥ 4 |
| `cpu-heavy` | ≥ 1 |
| `os-heavy` | ≥ 1 |

> If the pool doesn’t contain a tag, mark **N/A** and omit its quota.

---

## 2) Candidate Tag Mapping (All Candidates)

**Legend:**  
Interaction tags = `tool_only`, `slash_command`, `event_hook`, `ui_integration`, `provider`, `input_transform`  
Capabilities = `read`, `write`, `exec`, `http`, `env`  
Runtime = `legacy-js`, `multi-file`, `pkg-with-deps`, `provider-ext`, `gist`, `pi-package`  
I/O = `fs-heavy`, `network-heavy`, `ui-centric`, `cpu-heavy`, `os-heavy`

### A) pi‑mono example extensions

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `permission-gate.ts` | legacy-js | event_hook, ui_integration | exec, env | medium | ui-centric |
| `protected-paths.ts` | legacy-js | event_hook | write, read | small | fs-heavy |
| `confirm-destructive.ts` | legacy-js | slash_command, ui_integration | env | small | ui-centric |
| `dirty-repo-guard.ts` | legacy-js | event_hook | exec | small | fs-heavy |
| `sandbox/` | multi-file | event_hook | exec | large | os-heavy |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `todo.ts` | legacy-js | tool_only, slash_command, ui_integration | write, read | medium | fs-heavy |
| `hello.ts` | legacy-js | tool_only | env | small | ui-centric |
| `question.ts` | legacy-js | tool_only, ui_integration | env | small | ui-centric |
| `questionnaire.ts` | legacy-js | tool_only, ui_integration | env | medium | ui-centric |
| `tool-override.ts` | legacy-js | event_hook, tool_only | read, write | medium | fs-heavy |
| `truncated-tool.ts` | legacy-js | tool_only | exec | medium | fs-heavy |
| `antigravity-image-gen.ts` | legacy-js | tool_only | http, write | medium | network-heavy |
| `ssh.ts` | legacy-js | tool_only | exec, http | large | network-heavy |
| `subagent/` | multi-file | tool_only | exec | large | cpu-heavy |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `preset.ts` | legacy-js | slash_command, ui_integration | env | medium | ui-centric |
| `plan-mode/` | multi-file | slash_command, ui_integration | read | large | ui-centric |
| `tools.ts` | legacy-js | slash_command, ui_integration | env | medium | ui-centric |
| `handoff.ts` | legacy-js | slash_command | write | medium | fs-heavy |
| `qna.ts` | legacy-js | slash_command, ui_integration | env | small | ui-centric |
| `status-line.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `widget-placement.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `model-status.ts` | legacy-js | event_hook, ui_integration | env | small | ui-centric |
| `snake.ts` | legacy-js | ui_integration | env | large | cpu-heavy |
| `space-invaders.ts` | legacy-js | ui_integration | env | large | cpu-heavy |
| `send-user-message.ts` | legacy-js | slash_command | env | small | ui-centric |
| `timed-confirm.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `rpc-demo.ts` | legacy-js | ui_integration | env | medium | ui-centric |
| `modal-editor.ts` | legacy-js | ui_integration | env | large | ui-centric |
| `rainbow-editor.ts` | legacy-js | ui_integration | env | medium | ui-centric |
| `notify.ts` | legacy-js | event_hook, ui_integration | exec | medium | os-heavy |
| `titlebar-spinner.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `summarize.ts` | legacy-js | slash_command, tool_only | http | medium | network-heavy |
| `custom-footer.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `custom-header.ts` | legacy-js | ui_integration | env | small | ui-centric |
| `overlay-test.ts` | legacy-js | ui_integration | env | medium | ui-centric |
| `overlay-qa-tests.ts` | legacy-js | ui_integration | env | large | ui-centric |
| `doom-overlay/` | multi-file | ui_integration | exec? | large | cpu-heavy |
| `shutdown-command.ts` | legacy-js | slash_command | env | small | ui-centric |
| `interactive-shell.ts` | legacy-js | event_hook | exec | medium | os-heavy |
| `inline-bash.ts` | legacy-js | input_transform | exec | medium | os-heavy |
| `bash-spawn-hook.ts` | legacy-js | event_hook | exec | small | os-heavy |
| `input-transform.ts` | legacy-js | event_hook | env | small | ui-centric |
| `system-prompt-header.ts` | legacy-js | event_hook | env | small | ui-centric |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `git-checkpoint.ts` | legacy-js | event_hook | exec | medium | fs-heavy |
| `auto-commit-on-exit.ts` | legacy-js | event_hook | exec | medium | fs-heavy |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `pirate.ts` | legacy-js | event_hook | env | small | ui-centric |
| `claude-rules.ts` | legacy-js | event_hook | read | medium | fs-heavy |
| `custom-compaction.ts` | legacy-js | event_hook | env | medium | ui-centric |
| `trigger-compact.ts` | legacy-js | slash_command | env | small | ui-centric |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `mac-system-theme.ts` | legacy-js | event_hook | env | small | os-heavy |
| `dynamic-resources/` | multi-file | event_hook | read | medium | fs-heavy |
| `message-renderer.ts` | legacy-js | ui_integration | env | medium | ui-centric |
| `event-bus.ts` | legacy-js | event_hook | env | medium | ui-centric |
| `session-name.ts` | legacy-js | event_hook | env | small | ui-centric |
| `bookmark.ts` | legacy-js | event_hook | env | small | ui-centric |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `custom-provider-anthropic/` | provider-ext | provider | http | large | network-heavy |
| `custom-provider-gitlab-duo/` | provider-ext | provider | http | large | network-heavy |
| `custom-provider-qwen-cli/` | provider-ext | provider | exec, http | large | network-heavy |

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `with-deps/` | pkg-with-deps | mixed | read, write | medium | fs-heavy |
| `file-trigger.ts` | legacy-js | event_hook | read | small | fs-heavy |

### B) Repo‑local `.pi/extensions`

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `.pi/extensions/diff.ts` | legacy-js | slash_command, ui_integration | exec | medium | fs-heavy |
| `.pi/extensions/files.ts` | legacy-js | slash_command, ui_integration | read | small | fs-heavy |
| `.pi/extensions/prompt-url-widget.ts` | legacy-js | ui_integration | http | medium | network-heavy |
| `.pi/extensions/redraws.ts` | legacy-js | ui_integration | env | small | ui-centric |

### C) badlogic gists

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `review-extension*.ts` | gist | slash_command, ui_integration | write | medium | fs-heavy |
| `diff.ts` | gist | slash_command, ui_integration | exec | medium | fs-heavy |

### D) Community / npm / git packages

| Candidate | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|
| `agentsbox` | pi-package | tool_only | exec, http | medium | network-heavy |
| `pi-doom` | pi-package | ui_integration | exec | large | cpu-heavy |

---

## 3) How to Apply the Matrix (for bd‑ic9)

1. Rank candidates by score (per `CONFORMANCE.md`) within each source tier.  
2. Select built‑ins/official examples first, then fill by rank to target size.  
3. Enforce diversity quotas above by swapping in the highest‑ranked candidate that covers a missing tag.  
4. Document any quota exceptions with rationale.

