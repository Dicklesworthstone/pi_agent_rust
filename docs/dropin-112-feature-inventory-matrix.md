# DROPIN-112: Feature Inventory Matrix — Pi TypeScript vs Pi Rust

> **Bead:** bd-w9i9o | **Status:** in_progress | **Priority:** P0
> **Author:** CobaltElk (claude-opus-4-6) | **Date:** 2026-02-14
> **Purpose:** Enumerate every mode, flag, command, event, config knob, API surface, and integration contract across both implementations.

---

## Legend

| Symbol | Meaning |
|--------|---------|
| Y | Implemented and functional |
| P | Partial — exists but incomplete or divergent |
| N | Not implemented |
| X | Not applicable (design decision, not a gap) |
| ? | Unknown / needs investigation |

---

## 1. CLI Flags & Options

### Model Configuration

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--provider <name>` | Y | Y | env: PI_PROVIDER in both |
| `--model <id>` | Y | Y | env: PI_MODEL in both |
| `--api-key <key>` | Y | Y | Overrides env vars |
| `--models <patterns>` | Y | Y | Ctrl+P cycling, comma-separated globs |
| `--list-models [search]` | Y | Y | Optional fuzzy search pattern |
| `--list-providers` | N | Y | Rust-only addition |

### Thinking / Reasoning

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--thinking <level>` | Y | Y | off/minimal/low/medium/high/xhigh |

### System Prompt

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--system-prompt <text>` | Y | Y | Override system prompt |
| `--append-system-prompt <text>` | Y | Y | Append text or file path |

### Session Management

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `-c, --continue` | Y | Y | Continue previous session |
| `-r, --resume` | Y | Y | Session picker UI |
| `--session <path>` | Y | Y | Specific session file |
| `--session-dir <dir>` | Y | Y | Session storage directory |
| `--no-session` | Y | Y | Ephemeral mode |

### Mode & Output

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--mode text` | Y | Y | Text output mode |
| `--mode json` | Y | Y | JSON events mode |
| `--mode rpc` | Y | Y | JSON-RPC protocol mode |
| `-p, --print` | Y | Y | Non-interactive mode |
| `--verbose` | Y | Y | Force verbose startup |

### Tools

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--no-tools` | Y | Y | Disable all built-in tools |
| `--tools <list>` | Y | Y | Default: read,bash,edit,write |

### Extensions

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `-e, --extension <path>` | Y | Y | Load extension (repeatable) |
| `--no-extensions` | Y | Y | Disable discovery |
| `--extension-policy <profile>` | N | Y | Rust-only: safe/balanced/permissive |
| `--explain-extension-policy` | N | Y | Rust-only: print policy and exit |
| `--repair-policy <mode>` | N | Y | Rust-only: off/suggest/auto-safe/auto-strict |
| `--explain-repair-policy` | N | Y | Rust-only: print repair policy and exit |

### Skills & Prompt Templates

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--skill <path>` | Y | Y | Load skill (repeatable) |
| `--no-skills` | Y | Y | Disable skill discovery |
| `--no-context-files` | Y | Y | Disable AGENTS.md / CLAUDE.md discovery |
| `--prompt-template <path>` | Y | Y | Load template (repeatable) |
| `--no-prompt-templates` | Y | Y | Disable template discovery |

### Themes

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--theme <name>` | Y | Y | Select active theme |
| `--theme-path <dir>` | N | Y | Rust-only: add theme discovery path |
| `--no-themes` | Y | Y | Disable theme discovery |

### Export & Info

| Flag | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| `--export <path>` | Y | Y | Export session to HTML |
| `--help, -h` | Y | Y | Show help |
| `--version, -v` | Y | Y | Show version |

### Positional Arguments

| Feature | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `@file` references | Y | Y | Include file contents |
| Message arguments | Y | Y | Non-@ strings become messages |

---

## 2. Subcommands (Package Management)

| Command | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `install <source> [-l]` | Y | Y | npm:/git:/local sources |
| `remove <source> [-l]` | Y | Y | Remove from settings |
| `update [source]` | Y | Y | Update all or specific |
| `list` | Y | Y | List installed packages |
| `config` | Y | Y | Open configuration |
| `update-index` | N | Y | Rust-only: refresh extension index cache |
| `info <name>` | N | Y | Rust-only: extension details |
| `search <query>` | N | Y | Rust-only: search extensions |
| `doctor [path]` | N | Y | Rust-only: diagnose environment health |

---

## 3. Execution Modes

| Mode | TS Pi | Rust Pi | Notes |
|------|-------|---------|-------|
| Interactive (default TUI) | Y | Y | Full terminal UI |
| Print (`-p`) | Y | Y | Single-shot, non-interactive |
| RPC (`--mode rpc`) | Y | Y | JSON protocol over stdin/stdout |
| JSON (`--mode json`) | Y | Y | JSON event lines |
| Text (`--mode text`) | Y | Y | Plain text output |

---

## 4. Built-in Tools

| Tool | TS Pi | Rust Pi | Default? | Notes |
|------|-------|---------|----------|-------|
| `read` | Y | Y | Y | File/image reading |
| `bash` | Y | Y | Y | Shell command execution |
| `edit` | Y | Y | Y | String replacement editing |
| `write` | Y | Y | Y | File creation/overwrite |
| `grep` | Y | Y | N | Content search with context |
| `find` | Y | Y | N | File discovery by pattern |
| `ls` | Y | Y | N | Directory listing |

### Tool Limits

| Limit | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| DEFAULT_MAX_LINES | 2000 | 2000 | Match |
| DEFAULT_MAX_BYTES | 1,000,000 | 1,000,000 | Match |
| GREP_MAX_LINE_LENGTH | ? | 500 | Needs TS verification |
| DEFAULT_GREP_LIMIT | ? | 100 | Needs TS verification |
| DEFAULT_FIND_LIMIT | ? | 1000 | Needs TS verification |
| DEFAULT_LS_LIMIT | ? | 500 | Needs TS verification |
| DEFAULT_BASH_TIMEOUT_SECS | ? | 120 | Needs TS verification |
| IMAGE_MAX_BYTES | ? | 4.5MB | Needs TS verification |
| READ_TOOL_MAX_BYTES | ? | 100MB | Needs TS verification |

---

## 5. Slash Commands (Interactive Mode)

| Command | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `/settings` | Y | ? | TS settings menu |
| `/model` | Y | Y | Model selector |
| `/scoped-models` | Y | ? | Enable/disable models for cycling |
| `/export` | Y | Y | Export session to HTML |
| `/share` | Y | ? | Share as GitHub gist |
| `/copy` | Y | ? | Copy last message to clipboard |
| `/name` | Y | ? | Set session name |
| `/session` | Y | ? | Session info/stats |
| `/changelog` | Y | ? | Show changelog |
| `/hotkeys` | Y | ? | Show keybindings |
| `/fork` | Y | ? | Fork from previous message |
| `/tree` | Y | Y | Navigate session tree |
| `/login` | Y | ? | OAuth login |
| `/logout` | Y | ? | OAuth logout |
| `/new` | Y | ? | Start new session |
| `/compact` | Y | Y | Manual compaction |
| `/resume` | Y | ? | Resume different session |
| `/reload` | Y | ? | Reload extensions/skills/prompts/themes |
| `/help` | Y | Y | Show help |
| `/clear` | Y | Y | Clear message |
| `/exit` | Y | Y | Exit application |
| Dynamic prompt templates | Y | Y | Registered as `/template-name` |
| Dynamic skill commands | Y | Y | Registered as `/skill:name` |
| Extension commands | Y | Y | Registered dynamically |

---

## 6. Configuration Settings (settings.json)

### Appearance

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `theme` | Y | Y | Theme name/path |
| `hideThinkingBlock` | Y | Y | Hide thinking in TUI |
| `showHardwareCursor` | Y | Y | Show hardware cursor |

### Model Defaults

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `defaultProvider` | Y | Y | Default LLM provider |
| `defaultModel` | Y | Y | Default model ID |
| `defaultThinkingLevel` | Y | Y | Default thinking level |
| `enabledModels` | Y | Y | Model patterns for cycling |

### Message Queue

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `steeringMode` | Y | Y | all / one-at-a-time |
| `followUpMode` | Y | Y | all / one-at-a-time |

### Terminal Behavior

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `quietStartup` | Y | Y | Don't show changelog |
| `collapseChangelog` | Y | Y | Collapse by default |
| `lastChangelogVersion` | Y | Y | Track shown version |
| `doubleEscapeAction` | Y | Y | fork/tree/none |
| `editorPaddingX` | Y | Y | Editor padding |
| `autocompleteMaxVisible` | Y | Y | Max autocomplete suggestions |
| `sessionPickerInput` | N | Y | Rust-only: non-interactive picker |
| `sessionStore` | N | Y | Rust-only: jsonl/sqlite backend |

### Compaction

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `compaction.enabled` | Y | Y | Enable compaction |
| `compaction.reserveTokens` | Y (16384) | Y (16384) | Match |
| `compaction.keepRecentTokens` | Y (20000) | Y (20000) | Match |

### Branch Summary

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `branchSummary.reserveTokens` | Y (16384) | Y | Match |

### Retry

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `retry.enabled` | Y | Y | Enable auto retry |
| `retry.maxRetries` | Y (3) | Y (3) | Match |
| `retry.baseDelayMs` | Y (2000) | Y (2000) | Match |
| `retry.maxDelayMs` | Y (60000) | Y (60000) | Match |

### Shell

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `shellPath` | Y | Y | Shell executable path |
| `shellCommandPrefix` | Y | Y | Prefix for commands |
| `ghPath` | N | Y | Rust-only: GitHub CLI path |

### Images

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `images.autoResize` | Y | Y | Auto-resize large images |
| `images.blockImages` | Y | Y | Block all images |

### Terminal Display

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `terminal.showImages` | Y | Y | Show images |
| `terminal.clearOnShrink` | Y | Y | Clear on shrink |

### Thinking Budgets

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `thinkingBudgets` | Y | Y | Per-level token budgets |

### Packages & Resources

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `packages` | Y | Y | Package sources |
| `extensions` | Y | Y | Extension paths |
| `skills` | Y | Y | Skill paths |
| `prompts` | Y | Y | Prompt paths |
| `themes` | Y | Y | Theme paths |
| `enableSkillCommands` | Y | Y | Enable /skill commands |

### Markdown

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `markdown.codeBlockIndent` | Y | Y | Verified (rendered indentation) |

### Extension Policy (Rust-only)

| Setting | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `extensionPolicy.profile` | N | Y | safe/balanced/permissive |
| `extensionPolicy.allowDangerous` | N | Y | Allow dangerous caps |
| `repairPolicy.mode` | N | Y | off/suggest/auto-safe/auto-strict |
| `extensionRisk.enabled` | N | Y | Runtime risk controller |
| `extensionRisk.alpha` | N | Y | Type-I error target |
| `extensionRisk.windowSize` | N | Y | Sliding window |
| `extensionRisk.ledgerLimit` | N | Y | Max entries |
| `extensionRisk.decisionTimeoutMs` | N | Y | Decision timeout |
| `extensionRisk.failClosed` | N | Y | Fail closed behavior |
| `extensionRisk.enforce` | N | Y | Enforce decisions |

---

## 7. Environment Variables

### Provider API Keys

| Variable | TS Pi | Rust Pi | Notes |
|----------|-------|---------|-------|
| `ANTHROPIC_API_KEY` | Y | Y | |
| `ANTHROPIC_OAUTH_TOKEN` | Y | ? | Needs verification in Rust |
| `OPENAI_API_KEY` | Y | Y | |
| `GOOGLE_API_KEY` / `GEMINI_API_KEY` | Y | Y | TS uses GEMINI_, Rust uses GOOGLE_ |
| `AZURE_OPENAI_API_KEY` | Y | Y | |
| `AZURE_OPENAI_BASE_URL` | Y | ? | Needs verification |
| `AZURE_OPENAI_RESOURCE_NAME` | Y | ? | Needs verification |
| `AZURE_OPENAI_API_VERSION` | Y | ? | Needs verification |
| `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` | Y | ? | Needs verification |
| `AWS_ACCESS_KEY_ID` | Y | Y | Bedrock |
| `AWS_SECRET_ACCESS_KEY` | Y | Y | Bedrock |
| `AWS_BEARER_TOKEN_BEDROCK` | Y | ? | Needs verification |
| `AWS_REGION` | Y | ? | Needs verification |
| `AWS_PROFILE` | Y | ? | Needs verification |
| `GROQ_API_KEY` | Y | Y | |
| `CEREBRAS_API_KEY` | Y | Y | |
| `XAI_API_KEY` | Y | Y | |
| `OPENROUTER_API_KEY` | Y | Y | |
| `MISTRAL_API_KEY` | Y | Y | |
| `TOGETHER_API_KEY` | Y | Y | |
| `DEEPSEEK_API_KEY` | Y | Y | |
| `PERPLEXITY_API_KEY` | Y | Y | |
| `COHERE_API_KEY` | N | Y | Rust-only provider |
| `AI_GATEWAY_API_KEY` | Y | ? | Vercel AI Gateway |
| `ZAI_API_KEY` | Y | ? | ZAI provider |
| `MINIMAX_API_KEY` | Y | ? | MiniMax provider |
| `KIMI_API_KEY` | Y | ? | Kimi provider |
| `MOONSHOT_API_KEY` | N | Y | Rust-only |
| `DASHSCOPE_API_KEY` | N | Y | Rust-only (Qwen) |
| `FIREWORKS_API_KEY` | N | Y | Rust-only |
| `DEEPINFRA_API_KEY` | N | Y | Rust-only |
| `GITLAB_API_TOKEN` | N | Y | Rust-only (GitLab Duo) |

### Configuration Variables

| Variable | TS Pi | Rust Pi | Notes |
|----------|-------|---------|-------|
| `PI_CODING_AGENT_DIR` | Y | Y | Config root |
| `PI_PACKAGE_DIR` | Y | Y | Package directory |
| `PI_SESSIONS_DIR` | N | Y | Rust-only |
| `PI_CONFIG_PATH` | N | Y | Rust-only |
| `PI_SHARE_VIEWER_URL` | Y | ? | Share viewer base URL |

### Development / Testing

| Variable | TS Pi | Rust Pi | Notes |
|----------|-------|---------|-------|
| `PI_TEST_MODE` | Y | Y | Deterministic rendering |
| `PI_TIMING` | Y | ? | Timing output |
| `PI_SKIP_VERSION_CHECK` | Y | ? | Skip version check |
| `PI_HARDWARE_CURSOR` | Y | ? | Hardware cursor |
| `PI_CLEAR_ON_SHRINK` | Y | ? | Clear on shrink |
| `VCR_MODE` | N | Y | Rust-only VCR testing |
| `VCR_CASSETTE_DIR` | N | Y | Rust-only VCR testing |
| `PI_VCR_TEST_NAME` | N | Y | Rust-only VCR testing |
| `PI_EXTENSION_ALLOW_DANGEROUS` | N | Y | Rust-only |
| `PI_REPAIR_POLICY` | N | Y | Rust-only |
| `PI_EXT_COMPAT_SCAN` | N | Y | Rust-only |

---

## 8. Providers

| Provider | TS Pi | Rust Pi | Notes |
|----------|-------|---------|-------|
| Anthropic (Claude) | Y | Y | Primary provider |
| OpenAI (GPT) | Y | Y | Chat Completions API |
| OpenAI Responses | Y | Y | Responses API |
| Google Gemini | Y | Y | |
| Azure OpenAI | Y | Y | |
| Amazon Bedrock | Y | Y | |
| Google Vertex AI | Y | Y | |
| Groq | Y | Y | |
| Cerebras | Y | Y | |
| xAI (Grok) | Y | Y | |
| OpenRouter | Y | Y | |
| Mistral | Y | Y | |
| Together AI | Y | Y | |
| DeepSeek | Y | Y | |
| Perplexity | Y | Y | |
| Cohere | N | Y | Rust-only |
| GitLab Duo | N | Y | Rust-only |
| GitHub Copilot | Y | Y | |
| Ollama | N | Y | Rust-only (local) |
| Vercel AI Gateway | Y | ? | Needs verification |
| ZAI | Y | ? | Needs verification |
| MiniMax | Y | ? | Needs verification |
| Kimi | Y | ? | Needs verification |
| Moonshot | N | Y | Rust-only |
| DashScope/Qwen | N | Y | Rust-only |
| Fireworks | N | Y | Rust-only |
| DeepInfra | N | Y | Rust-only |
| Extension providers | Y | Y | Via streamSimple bridge |

---

## 9. Agent Events

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `agent_start` | Y | Y | Agent loop start |
| `agent_end` | Y | Y | Agent loop end |
| `turn_start` | Y | Y | Turn start |
| `turn_end` | Y | Y | Turn end |
| `content_block_start` | Y | Y | Content block started |
| `content_block_delta` | Y | Y | Content block delta |
| `content_block_end` | Y | Y | Content block end |
| `tool_call_start` | Y | Y | Tool call started |
| `tool_call_end` | Y | Y | Tool call end |
| `tool_execution_start` | Y | Y | Tool execution start |
| `tool_execution_update` | Y | Y | Tool execution streaming update |
| `tool_execution_end` | Y | Y | Tool execution end |
| `message` | Y | Y | Message added |
| `message_update` | Y | Y | Message updated |
| `error` | Y | Y | Error occurred |
| `auto_compaction_start` | Y | Y | Auto-compaction lifecycle start |
| `auto_compaction_end` | Y | Y | Auto-compaction lifecycle end |
| `auto_retry_start` | Y | Y | Auto-retry lifecycle start |
| `auto_retry_end` | Y | Y | Auto-retry lifecycle end |

---

## 10. Extension Events (Hook Points)

### Session Events

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `session_start` | Y | Y | Initial session load |
| `session_before_switch` | Y | ? | Cancellable |
| `session_switch` | Y | ? | After switching |
| `session_before_fork` | Y | ? | Cancellable |
| `session_fork` | Y | ? | After forking |
| `session_before_compact` | Y | ? | Cancellable, customizable |
| `session_compact` | Y | ? | After compaction |
| `session_before_tree` | Y | ? | Cancellable |
| `session_tree` | Y | ? | After tree navigation |
| `session_shutdown` | Y | Y | On exit |
| `resources_discover` | Y | ? | Resource discovery |

### Agent Events

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `context` | Y | Y | Before LLM call (can modify) |
| `before_agent_start` | Y | ? | Cancellable |
| `agent_start` | Y | Y | Loop start |
| `agent_end` | Y | Y | Loop end |
| `turn_start` | Y | Y | Turn start |
| `turn_end` | Y | Y | Turn end |
| `model_select` | Y | ? | Model selection |

### Tool Events

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `tool_call` | Y | Y | Before execution (can block) |
| `tool_result` | Y | Y | After execution (can modify) |

### User Events

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `user_bash` | Y | ? | User shell with ! prefix |
| `input` | Y | ? | User input (can transform) |

---

## 11. RPC Protocol Commands

| Command | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `prompt` | Y | ? | Initial prompt |
| `steer` | Y | Y | Steer with user message |
| `follow_up` / `queue-follow-up` | Y | Y | Queue follow-up |
| `abort` | Y | Y | Abort current operation |
| `new_session` | Y | ? | Start new session |
| `get_state` / `get-state` | Y | Y | Get session state |
| `set_model` / `set-model` | Y | Y | Set active model |
| `cycle_model` | Y | ? | Cycle to next model |
| `get_available_models` | Y | ? | List models |
| `set_thinking_level` | Y | ? | Set thinking level |
| `cycle_thinking_level` | Y | ? | Cycle thinking level |
| `set_steering_mode` | Y | ? | Set steering mode |
| `set_follow_up_mode` | Y | ? | Set follow-up mode |
| `compact` | Y | Y | Compact session |
| `set_auto_compaction` / `set-auto-compaction` | Y | Y | Enable/disable compaction |
| `set_auto_retry` / `set-auto-retry` | Y | Y | Enable/disable retry |
| `abort_retry` | Y | ? | Abort retry |
| `bash` | Y | ? | Execute bash |
| `abort_bash` | Y | ? | Abort bash |
| `get_session_stats` | Y | ? | Session statistics |
| `export_html` | Y | ? | Export to HTML |
| `switch_session` | Y | ? | Switch session |
| `fork` | Y | ? | Fork from entry |
| `get_fork_messages` | Y | ? | Get fork messages |
| `get_last_assistant_text` | Y | ? | Last assistant text |
| `set_session_name` | Y | ? | Set name |
| `get_messages` | Y | ? | Get all messages |
| `get_commands` | Y | ? | List commands |
| `query-completion` | N | Y | Rust-only: completion query |

### RPC Events (responses)

| Event | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| Agent events (streamed) | Y | Y | All agent events |
| `extension_ui_request` | Y | Y | Covered by `tests/e2e_rpc.rs` + `tests/json_mode_parity.rs` |
| `extension_ui_response` | Y | Y | Covered by `tests/e2e_rpc.rs` (success + negative paths) |
| `extension_error` | N | Y | Rust-only event emitted on extension dispatch/runtime failures |

---

## 12. Session Entry Types

| Entry Type | TS Pi | Rust Pi | Notes |
|------------|-------|---------|-------|
| `session` (header) | Y | Y | Version 3 format |
| `message` | Y | Y | Chat messages |
| `model_change` | Y | Y | Model changes |
| `thinking_level_change` | Y | Y | Thinking level changes |
| `compaction` | Y | Y | Context compaction |
| `branch_summary` | Y | Y | Branch summaries |
| `custom` | Y | Y | Extension data |
| `label` | N | Y | Rust-only: session labels |
| `branch` | N | Y | Rust-only: branch markers |
| `note` | N | Y | Rust-only: custom notes |

---

## 13. Thinking Levels

| Level | TS Pi | Rust Pi | Notes |
|-------|-------|---------|-------|
| `off` | Y | Y | No thinking |
| `minimal` | Y | Y | Light reasoning |
| `low` | Y | Y | Low reasoning |
| `medium` | Y (default) | Y (default) | Balanced reasoning |
| `high` | Y | Y | Deep reasoning |
| `xhigh` | Y | Y | Maximum reasoning |

### Aliases (Rust-only)

| Alias | Maps to |
|-------|---------|
| `none`, `0` | off |
| `min` | minimal |
| `1` | low |
| `med`, `2` | medium |
| `3` | high |
| `4` | xhigh |

---

## 14. Key Bindings (Interactive Mode)

| Action | TS Pi | Rust Pi | Default Key | Notes |
|--------|-------|---------|-------------|-------|
| Interrupt | Y | Y | Escape | |
| Clear | Y | Y | Ctrl+C | |
| Exit | Y | Y | Ctrl+D | When empty |
| Suspend | Y | Y | Ctrl+Z | |
| Cycle thinking | Y | Y | Shift+Tab | |
| Cycle model forward | Y | Y | Ctrl+P | |
| Cycle model backward | Y | Y | Shift+Ctrl+P | |
| Select model | Y | Y | Ctrl+L | |
| Expand tools | Y | ? | Ctrl+O | Needs verification |
| Toggle thinking | Y | ? | Ctrl+T | Needs verification |
| Toggle session named filter | Y | ? | Ctrl+N | Needs verification |
| External editor | Y | ? | Ctrl+G | Needs verification |
| Follow up | Y | Y | Alt+Enter | |
| Dequeue | Y | ? | Alt+Up | Needs verification |
| Paste image | Y | ? | Ctrl+V | Needs verification |
| New session | Y | ? | (none) | Needs verification |
| Tree | Y | ? | (none) | Needs verification |
| Fork | Y | ? | (none) | Needs verification |

### Customization

| Feature | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| `keybindings.json` | Y | Y | `~/.pi/agent/keybindings.json` |

---

## 15. Extension API Surface

### Registration

| API | TS Pi | Rust Pi | Notes |
|-----|-------|---------|-------|
| `registerTool()` | Y | Y | Register custom tool |
| `registerCommand()` | Y | Y | Register slash command |
| `registerShortcut()` | Y | Y | Register keybinding |
| `registerFlag()` | Y | Y | Register extension flag |
| `registerProvider()` | Y | Y | Register custom provider |

### Session

| API | TS Pi | Rust Pi | Notes |
|-----|-------|---------|-------|
| `getState()` | Y | Y | Session state |
| `getMessages()` | Y | Y | Current messages |
| `setSessionName()` | Y | Y | Set session name |
| `setModel()` | Y | Y | Change model |
| `setLabel()` | Y | Y | Label an entry |
| `sendMessage()` | Y | Y | Send message |
| `appendEntry()` | Y | Y | Add custom entry |
| `getActiveTools()` | Y | Y | Active tool list |
| `getAllTools()` | Y | Y | All tools |
| `setActiveTools()` | Y | Y | Set active tools |
| `getThinkingLevel()` | Y | Y | Current thinking |
| `setThinkingLevel()` | Y | Y | Set thinking |

### UI

| API | TS Pi | Rust Pi | Notes |
|-----|-------|---------|-------|
| `ui.select()` | Y | Y | Selection dialog |
| `ui.confirm()` | Y | Y | Confirmation dialog |
| `ui.input()` | Y | Y | Input dialog |
| `ui.notify()` | Y | Y | Notification |
| `ui.setStatus()` | Y | Y | Status bar |
| `ui.setWorkingMessage()` | Y | ? | Working message |
| `ui.setWidget()` | Y | Y | Custom widget |
| `ui.setFooter()` | Y | ? | Custom footer |
| `ui.setHeader()` | Y | ? | Custom header |
| `ui.setTitle()` | Y | Y | Window title |
| `ui.custom()` | Y | ? | Custom component |
| `ui.setEditorText()` | Y | ? | Set editor text |
| `ui.getEditorText()` | Y | ? | Get editor text |
| `ui.editor()` | Y | ? | Full editor dialog |
| `ui.theme` | Y | Y | Current theme |
| `ui.getAllThemes()` | Y | ? | Theme list |
| `ui.getTheme()` | Y | ? | Get theme by name |
| `ui.setTheme()` | Y | ? | Set active theme |

### Hostcalls

| API | TS Pi | Rust Pi | Notes |
|-----|-------|---------|-------|
| `exec()` | Y | Y | Execute shell command |
| `events` bus | Y | Y | Shared event bus |
| `fetch()` | Y | Y | HTTP fetch |
| `read()` | Y | Y | Read file |
| `write()` | Y | Y | Write file |
| `grep()` | Y | Y | Search files |
| `find()` | Y | Y | Find files |
| `ls()` | Y | Y | List directory |

### Capability Policy

| Feature | TS Pi | Rust Pi | Notes |
|---------|-------|---------|-------|
| Policy profiles | N | Y | Rust-only: safe/balanced/permissive |
| Per-capability audit | N | Y | Rust-only: fine-grained control |
| Risk controller | N | Y | Rust-only: statistical risk |

---

## 16. Interactive UI Components

| Component | TS Pi | Rust Pi | Notes |
|-----------|-------|---------|-------|
| Model selector | Y | Y | |
| Scoped models selector | Y | ? | Needs verification |
| Thinking selector | Y | Y | |
| Session selector/picker | Y | Y | |
| Tree selector | Y | Y | |
| Settings selector | Y | ? | Needs verification |
| Login dialog | Y | ? | OAuth flow |
| Config selector | Y | ? | Package resource config |
| Tool execution display | Y | Y | |
| Bash execution display | Y | Y | |
| Skill invocation display | Y | Y | |
| Extension editor | Y | ? | Custom UI |
| Autocomplete | Y | Y | @file and /commands |

---

## Summary of Divergences

### Features in TS Pi but missing/unclear in Rust Pi

1. **Agent events**: `auto_compaction_start/end`, `auto_retry_start/end` (bd-2ilgm addresses this)
2. **RPC commands**: Many TS RPC commands unverified (`cycle_model`, `set_thinking_level`, `cycle_thinking_level`, `bash`, `abort_bash`, `get_session_stats`, `fork`, `get_messages`, etc.)
3. **Extension events**: Several hook points unverified (`session_before_*`, `model_select`, `user_bash`, `input`)
4. **Extension UI**: Several UI methods unverified (`setWorkingMessage`, `setFooter`, `setHeader`, `custom`, `setEditorText`, `editor`)
5. **Provider support**: Vercel AI Gateway, ZAI, MiniMax, Kimi — unclear if in Rust

### Features in Rust Pi not in TS Pi (Rust-only)

1. **CLI flags**: `--list-providers`, `--extension-policy`, `--explain-extension-policy`, `--repair-policy`, `--explain-repair-policy`, `--theme-path`
2. **Subcommands**: `update-index`, `info`, `search`, `doctor`
3. **Providers**: Cohere, GitLab Duo, Ollama, Moonshot, DashScope, Fireworks, DeepInfra
4. **Extension security**: Capability policy profiles, risk controller, repair policy
5. **Session**: Labels, branch markers, notes entry types; SQLite backend
6. **Config**: sessionStore, sessionPickerInput, ghPath, extensionPolicy, repairPolicy, extensionRisk
7. **Thinking aliases**: Numeric aliases (0-4) and short forms (min, med)
8. **VCR test infrastructure**: VCR_MODE, VCR_CASSETTE_DIR, PI_VCR_TEST_NAME

---

## Items Requiring Investigation (marked with ?)

Each `?` in this matrix represents a surface that exists in one implementation and needs verification in the other. A follow-up task should systematically resolve these unknowns by:
1. Grepping the Rust codebase for each TS feature name
2. Checking TS source for each Rust-only feature name
3. Updating this matrix with Y/N/P for each entry

Total unknowns to resolve: ~60 items
