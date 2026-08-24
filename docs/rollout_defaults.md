# Rollout Defaults & Tool Classification Matrix

> **Program:** `OMP-ADOPT` Tool Port Program  
> **Bead ID:** `bd-cv653.8.2`  
> **Schema:** `pi.rollout.defaults.v1`

---

## 1. Tool Tiers & Default Activation Policy

Tools are partitioned into two architectural tiers per `xdev` load mode principles:
1. **Essential (Default-On):** Lightweight, zero external heavy runtime dependencies, universal utility for coding agents.
2. **Discoverable / Opt-in:** Heavy external dependencies (e.g. Chromium, display server, TTS APIs), specialized security boundaries, or explicit feature activation.

| Tool Name | Module | Default State | Activation Mechanism | Category / Scope |
|-----------|--------|---------------|----------------------|------------------|
| `read` | `src/tools.rs`, `src/url_read.rs` | **Default-On** | Built-in | File & URL Reader |
| `write` | `src/tools.rs` | **Default-On** | Built-in | File Creation |
| `edit` | `src/tools.rs` | **Default-On** | Built-in | Substring Edit |
| `bash` | `src/tools.rs` | **Default-On** | Built-in | Shell Execution |
| `grep` | `src/tools.rs` | **Default-On** | Built-in | In-process Grep |
| `find` | `src/tools.rs` | **Default-On** | Built-in | In-process Find |
| `ls` | `src/tools.rs` | **Default-On** | Built-in | In-process Listing |
| `hashline_edit` | `src/tools.rs` | **Default-On** | Built-in | Hashline Edits |
| `ask` | `src/ask.rs` | **Default-On** | Built-in | Interactive Question Picker |
| `todo` | `src/todo.rs` | **Default-On** | Built-in | Task Phase Tracking |
| `web_search` | `src/web_search.rs` | **Default-On** | Built-in / `--tools` | Multi-provider Search |
| `lsp` | `src/tools.rs` | **Default-On** | Built-in / LSP server | Code Intelligence |
| `ast_grep`, `ast_edit` | `src/ast_tools.rs` | **Default-On** | Built-in | Structural Code Refactoring |
| `subagent` | `src/subagents.rs` | **Opt-in** | `--tools subagent` | Child Agent Delegation |
| `memory` | `src/tools.rs` | **Opt-in** | `[memory] enable=true` | SQLite Memory Bank |
| `github` | `src/github.rs` | **Opt-in** | `--tools github` | GitHub CLI Operations |
| `browser` | `src/browser.rs` | **Opt-in** | `[browser] enable_browser=true` | Headless Chromium Automation |
| `computer` | `src/computer.rs` | **Opt-in** | `[computer] enable_computer=true` | Desktop OS & Screen Automation |
| `inspect_image` | `src/media_tools.rs` | **Opt-in** | `[media] enable_media=true` | Vision Image Inspection |
| `generate_image` | `src/media_tools.rs` | **Opt-in** | `[media] enable_media=true` | Image Generation |
| `tts` | `src/media_tools.rs` | **Opt-in** | `[media] enable_media=true` | Speech Synthesis |
| `eval` | `src/eval.rs` | **Opt-in** | `[eval] enable=true` | Python/JS Kernel Execution |
| `security_scan` | `src/security_scan.rs` | **Opt-in** | `--tools security_scan` | SARIF Security Review |

---

## 2. Configuration Schema Updates (`docs/settings.md`)

Each opt-in tool family is governed by structured configuration blocks in `~/.config/pi/config.toml` or `.pi/config.toml`:

- `[media]`: Configures TTS voice, provider, image generation models, and media artifact directories.
- `[computer]`: Controls desktop automation permissions, screenshot directories, and interactive approval gates.
- `[browser]`: Configures headless Chromium executable paths, debugging ports, and domain allowlists.
- `[memory]`: Configures local SQLite vector and fact store paths.
