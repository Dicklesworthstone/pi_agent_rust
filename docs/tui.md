# Interactive Interface (TUI)

Pi's interactive mode provides a full-screen terminal UI for chatting, streaming
responses, and managing sessions.

## Layout

### Header
Shows high-level session context (current model, status, hints). Exact contents
may vary as the UI evolves.

### Conversation View
The main area shows the conversation history.
- **User messages**: Highlighted in accent color.
- **Assistant messages**: Rendered as Markdown.
- **Thinking blocks**: Muted and italicized.
- **Tool calls/results**: Structured blocks showing tool execution and output.

### Editor
The input area at the bottom.
- **Single-line + multi-line editing** (see shortcuts below).
- **Autocomplete** for `@file` references, `/commands`, and resource names.
- Paste and editing behaviors follow the configured keybindings.

### Footer
Displays session statistics and status.
- Token usage (input/output) and estimated cost.
- Editor mode hints (Single-line vs Multi-line).
- Current status messages.

## Display Controls

| Action | Shortcut | Description |
|--------|----------|-------------|
| **Toggle Thinking** | `Ctrl+T` | Hide/show thinking blocks to reduce noise. |
| **Scroll History** | `PageUp` / `PageDown` | Scroll conversation view. |

## Navigation & Overlays

### Keyboard shortcuts (`/hotkeys`)
Use `/hotkeys` to see the current shortcut list (including any user overrides
from `~/.pi/agent/keybindings.json`).

## Slash commands

Type a slash command into the editor (prefix with `/`) and press Enter.

`/help` is the authoritative, in-app list. This section documents the current
command surface implemented in `src/interactive.rs`.

| Command | Description |
|---------|-------------|
| `/help` (`/h`, `/?`) | Show help text and shortcut tips. |
| `/login [provider]` | OAuth login (currently: anthropic). |
| `/logout [provider]` | Remove stored OAuth credentials. |
| `/clear` (`/cls`) | Clear conversation view/history. |
| `/model [id|provider/id]` (`/m`) | Show or change the current model. |
| `/thinking [level]` (`/think`, `/t`) | Set thinking level (`off|minimal|low|medium|high|xhigh`). |
| `/scoped-models [patterns\|clear]` (`/scoped`) | Show or set model patterns used for Ctrl+P cycling. |
| `/history` (`/hist`) | Show input history. |
| `/export [path]` | Export conversation to HTML. |
| `/session` (`/info`) | Show session info (path, tokens, cost). |
| `/settings` | Open settings selector UI. |
| `/theme [name]` | List or switch themes (see `docs/themes.md`). |
| `/resume` (`/r`) | Pick and resume a previous session. |
| `/new` | Start a new session. |
| `/copy` (`/cp`) | Copy last assistant message to clipboard. |
| `/name <name>` | Set session display name. |
| `/hotkeys` (`/keys`, `/keybindings`) | Show keyboard shortcuts. |
| `/changelog` | Show changelog entries. |
| `/tree` | Show session branch tree summary. |
| `/fork [id\|index]` | Fork from a user message (default: last on current path). |
| `/compact [notes]` | Compact older context with optional instructions. |
| `/reload` | Reload skills/prompts from disk. |
| `/share` | Upload session HTML to a secret GitHub gist and show URL. |
| `/exit` (`/quit`, `/q`) | Exit Pi. |

### Model selection
- Use `/model` to switch models (by `provider/id` or fuzzy match).
- Some builds also define shortcuts like `Ctrl+L` (model selector) and `Ctrl+P`
  (cycle models). If a shortcut appears in `/hotkeys` but does nothing, it
  hasn’t been wired in that build yet.

### Session Picker (`/resume`)
Browse and resume previous sessions without restarting Pi.
- `Enter`: Select session
- `Ctrl+D`: Delete session (with confirmation)

### Tree Navigator (`/tree`)
Visualize the conversation branching structure.
- `Up` / `Down`: Navigate nodes
- `Enter`: Switch to selected node (forks if not a leaf)
- `Ctrl+U`: Toggle user-only view (hides assistant/tool noise)

### Settings (`/settings`)
Change configuration on the fly (Thinking levels, themes, message delivery mode).

## Message Queue

When Pi is busy generating a response or running tools, you can still type.

- **Queue Steering (`Enter`)**: Sends your message as a steering interrupt after
  the current step completes.
- **Queue Follow-up (`Alt+Enter`)**: Adds your message to the follow-up queue to
  be processed when the agent becomes idle.
- **Restore queued messages (`Alt+Up`)**: Pull queued messages back into the
  editor (useful if you queued something by mistake).

The queue is visible above the editor when not empty.
