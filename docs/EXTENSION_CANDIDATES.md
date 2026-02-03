## Raw Extension Candidate List (Unfiltered)

This is a **source-first, unfiltered** candidate inventory for extension sampling. It prioritizes **breadth of feature coverage** over popularity and will be refined by downstream sampling beads.

### Sources (where candidates come from)

1. **pi‑mono example extensions** (local repo snapshot; upstream commit snapshot for reference)  
   `legacy_pi_mono_code/pi-mono/packages/coding-agent/examples/extensions/README.md`  
   Upstream snapshot: https://upd.dev/badlogic/pi-mono/src/commit/c6fc084534d0091e6243bdcf929249e48c36c9e9/packages/coding-agent/examples/extensions/README.md  
   Repo: https://github.com/badlogic/pi-mono  

2. **pi‑mono local `.pi/extensions`** (seed extensions in repo)  
   `legacy_pi_mono_code/pi-mono/.pi/extensions/`  

3. **Official Pi site** (docs + packages)  
   https://buildwithpi.ai/  
   https://buildwithpi.ai/packages  

4. **badlogic GitHub gists (extensions)**  
   https://gist.github.com/badlogic  
   https://gist.github.com/badlogic/679b221a1749353a5be3f3134c120685  
   https://gist.github.com/badlogic/30aef35d686483ffce22cc2aad99f3ff  
   https://gist.github.com/badlogic/587bcbc5d1d2b4d1cf30a1d0756275b9  
   https://gist.github.com/badlogic/8273f2bff572272e1036887e0744c3c8  

5. **Community GitHub gists**  
   https://gist.github.com/nicobailon/ee8a65353b9103ad5d149e7eeb452b10  
   https://gist.github.com/aadishv/7615082df075519d6efd9de793aa860a  

6. **Community npm package w/ Pi extension integration**  
   https://www.npmjs.com/package/agentsbox  

> Note: npm “pi-package” keyword results and buildwithpi package listings are not enumerated here; source list provides where to search.

---

## Candidate Metadata Fields

- **Name/Path**: extension name or directory.
- **Source**: where it originates (examples, gist, npm, git).
- **Type**: file, package directory, gist, npm package.
- **Interaction Model**: tool, slash command, event hook, provider, UI‑only, or mixed.
- **Capabilities (likely)**: `read` / `write` / `exec` / `http` / `env` (approximate from descriptions).
- **I/O Pattern**: FS‑heavy, network‑heavy, CPU‑heavy, or UI‑centric.
- **Last update**: from source listing where available; otherwise TBD.
- **Notes**: short rationale for inclusion.

> Capabilities are **inferred from descriptions**. A static scan can refine this later.

---

## A) pi‑mono Example Extensions (local snapshot)

**Lifecycle & Safety**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `permission-gate.ts` | pi‑mono examples | file | event hook + UI | exec? | UI‑centric | Confirm dangerous bash commands. |
| `protected-paths.ts` | pi‑mono examples | file | event hook | write | FS‑heavy | Blocks writes to protected paths. |
| `confirm-destructive.ts` | pi‑mono examples | file | command + UI | env? | UI‑centric | Confirms destructive session actions. |
| `dirty-repo-guard.ts` | pi‑mono examples | file | event hook | exec | FS‑heavy | Prevents changes when git dirty. |
| `sandbox/` | pi‑mono examples | dir | tool hook + runtime | exec | FS/OS | OS‑level sandboxing. |

**Custom Tools**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `todo.ts` | pi‑mono examples | file | tool + command + UI | write | FS‑heavy | Todo tool + `/todos` with persistence. |
| `hello.ts` | pi‑mono examples | file | tool | none | UI‑centric | Minimal custom tool example. |
| `question.ts` | pi‑mono examples | file | tool + UI | env? | UI‑centric | `ctx.ui.select()` example. |
| `questionnaire.ts` | pi‑mono examples | file | tool + UI | env? | UI‑centric | Multi‑question UI flow. |
| `tool-override.ts` | pi‑mono examples | file | tool override | read/write | FS‑heavy | Wrap built‑ins for logging/ACL. |
| `truncated-tool.ts` | pi‑mono examples | file | tool | exec | FS‑heavy | Wrap ripgrep with truncation. |
| `antigravity-image-gen.ts` | pi‑mono examples | file | tool | http/write | network‑heavy | Image generation via HTTP. |
| `ssh.ts` | pi‑mono examples | file | tool | exec/http | network‑heavy | Delegate tools over SSH. |
| `subagent/` | pi‑mono examples | dir | tool + process | exec | CPU/FS | Delegates tasks to subagents. |

**Commands & UI**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `preset.ts` | pi‑mono examples | file | command | env | UI‑centric | Model/tool preset switching. |
| `plan-mode/` | pi‑mono examples | dir | command + UI | read | UI‑centric | Plan mode workflow. |
| `tools.ts` | pi‑mono examples | file | command + UI | env | UI‑centric | `/tools` enable/disable. |
| `handoff.ts` | pi‑mono examples | file | command | write | FS‑heavy | Handoff to new session. |
| `qna.ts` | pi‑mono examples | file | command + UI | env | UI‑centric | Extracts questions into editor. |
| `status-line.ts` | pi‑mono examples | file | UI | env | UI‑centric | Status updates. |
| `widget-placement.ts` | pi‑mono examples | file | UI | env | UI‑centric | Widget placement demo. |
| `model-status.ts` | pi‑mono examples | file | event hook + UI | env | UI‑centric | Model change status bar. |
| `snake.ts` | pi‑mono examples | file | UI | env | CPU/UI | Game w/ keyboard input. |
| `space-invaders.ts` | pi‑mono examples | file | UI | env | CPU/UI | Game w/ custom UI. |
| `send-user-message.ts` | pi‑mono examples | file | command | env | UI‑centric | Send user messages from extension. |
| `timed-confirm.ts` | pi‑mono examples | file | UI | env | UI‑centric | Abortable confirm/select dialogs. |
| `rpc-demo.ts` | pi‑mono examples | file | UI + RPC | env | UI‑centric | Exercises RPC UI methods. |
| `modal-editor.ts` | pi‑mono examples | file | UI | env | UI‑centric | Custom modal editor. |
| `rainbow-editor.ts` | pi‑mono examples | file | UI | env | UI‑centric | Animated editor content. |
| `notify.ts` | pi‑mono examples | file | UI | exec | OS‑heavy | Desktop notifications via OSC. |
| `titlebar-spinner.ts` | pi‑mono examples | file | UI | env | UI‑centric | Titlebar spinner animation. |
| `summarize.ts` | pi‑mono examples | file | command + tool | http | network‑heavy | Summarize with model call. |
| `custom-footer.ts` | pi‑mono examples | file | UI | env | UI‑centric | Footer customization. |
| `custom-header.ts` | pi‑mono examples | file | UI | env | UI‑centric | Header customization. |
| `overlay-test.ts` | pi‑mono examples | file | UI | env | UI‑centric | Overlay compositing tests. |
| `overlay-qa-tests.ts` | pi‑mono examples | file | UI | env | UI‑centric | Overlay QA suite. |
| `doom-overlay/` | pi‑mono examples | dir | UI | exec? | CPU/UI | Doom overlay @ 35 FPS. |
| `shutdown-command.ts` | pi‑mono examples | file | command | env | UI‑centric | `/quit` via `ctx.shutdown()`. |
| `interactive-shell.ts` | pi‑mono examples | file | event hook | exec | OS‑heavy | Interactive commands. |
| `inline-bash.ts` | pi‑mono examples | file | input transform | exec | OS‑heavy | `!{command}` expansion. |
| `bash-spawn-hook.ts` | pi‑mono examples | file | event hook | exec | OS‑heavy | Spawn hook for bash. |
| `input-transform.ts` | pi‑mono examples | file | event hook | env | UI‑centric | Input transformation. |
| `system-prompt-header.ts` | pi‑mono examples | file | prompt | env | UI‑centric | System prompt header. |

**Git Integration**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `git-checkpoint.ts` | pi‑mono examples | file | event hook | exec | FS‑heavy | Git stash checkpoints. |
| `auto-commit-on-exit.ts` | pi‑mono examples | file | lifecycle hook | exec | FS‑heavy | Auto‑commit on exit. |

**System Prompt & Compaction**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `pirate.ts` | pi‑mono examples | file | prompt | env | UI‑centric | `systemPromptAppend`. |
| `claude-rules.ts` | pi‑mono examples | file | prompt | read | FS‑heavy | Read `.claude/rules/`. |
| `custom-compaction.ts` | pi‑mono examples | file | compaction hook | env | UI‑centric | Custom compaction. |
| `trigger-compact.ts` | pi‑mono examples | file | command | env | UI‑centric | Trigger compaction on size. |

**System Integration / Resources / Messaging**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `mac-system-theme.ts` | pi‑mono examples | file | system integration | env | OS‑heavy | Sync theme with macOS. |
| `dynamic-resources/` | pi‑mono examples | dir | resource hook | read | FS‑heavy | `resources_discover`. |
| `message-renderer.ts` | pi‑mono examples | file | UI | env | UI‑centric | Custom message renderer. |
| `event-bus.ts` | pi‑mono examples | file | event hook | env | UI‑centric | Inter‑extension events. |
| `session-name.ts` | pi‑mono examples | file | session hook | env | UI‑centric | Set session name. |
| `bookmark.ts` | pi‑mono examples | file | session hook | env | UI‑centric | Bookmark entries. |

**Custom Providers**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `custom-provider-anthropic/` | pi‑mono examples | dir | provider | http | network‑heavy | Custom provider w/ OAuth. |
| `custom-provider-gitlab-duo/` | pi‑mono examples | dir | provider | http | network‑heavy | Provider via proxy. |
| `custom-provider-qwen-cli/` | pi‑mono examples | dir | provider | exec/http | network‑heavy | Qwen CLI provider. |

**External Dependencies**

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `with-deps/` | pi‑mono examples | dir | mixed | read/write | FS‑heavy | Package.json + deps. |
| `file-trigger.ts` | pi‑mono examples | file | event hook | read | FS‑heavy | Watches trigger file. |

---

## B) GitHub Gists (badlogic)

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `diff.ts` | https://gist.github.com/badlogic/679b221a1749353a5be3f3134c120685 | gist | command + UI | exec | FS‑heavy | `/diff` command w/ UI; last active 2026‑01‑23. |
| `review-extension-v3.ts` | https://gist.github.com/badlogic/30aef35d686483ffce22cc2aad99f3ff | gist | command + session ops | write | FS‑heavy | `/review` branch‑from‑root; created 2026‑01‑16; other versions exist (v2/v1/corrected). |

---

## B2) Community GitHub Gists

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `terminal-title.ts` | https://gist.github.com/nicobailon/ee8a65353b9103ad5d149e7eeb452b10 | gist | event hook + UI | env | UI‑centric | Terminal tab title/status extension; created 2026‑01‑15. |
| `claude-style.ts` | https://gist.github.com/aadishv/7615082df075519d6efd9de793aa860a | gist | UI | env | UI‑centric | Claude‑style UI tweaks; created 2026‑01‑25. |

---

## C) Repo-local `.pi/extensions` (legacy pi-mono)

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `.pi/extensions/diff.ts` | pi‑mono `.pi` | file | command + UI | exec | FS‑heavy | Local diff UI extension. |
| `.pi/extensions/files.ts` | pi‑mono `.pi` | file | command + UI | read | FS‑heavy | File browser helper. |
| `.pi/extensions/prompt-url-widget.ts` | pi‑mono `.pi` | file | UI | http | network‑heavy | URL preview widget. |
| `.pi/extensions/redraws.ts` | pi‑mono `.pi` | file | UI | env | UI‑centric | UI redraw debugging. |

---

## D) Community / npm / Git Packages

| Name/Path | Source | Type | Interaction Model | Capabilities (likely) | I/O Pattern | Notes |
|---|---|---|---|---|---|---|
| `agentsbox` | npm (agentsbox) | npm pkg | tool + MCP bridge | exec/http | network‑heavy | Installs a pi extension via `agentsbox setup pi`. |
| `pi-doom` | buildwithpi example | git pkg | UI overlay | exec | CPU/UI | Example git package install for pi (from official docs). |

---

## E) Notes & Next Steps

1. **Static capability scan**: parse each candidate to extract exact hostcall usage.  
2. **Enrich metadata**: add package.json name/version where present.  
3. **Sampling matrix**: use this list as input for `bd-22h` stratified selection.  
