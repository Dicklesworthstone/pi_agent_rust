# Keybindings

Pi supports configurable keybindings in interactive mode.

## Configuration

User keybindings are loaded from:
`~/.pi/agent/keybindings.json`

### Format

The configuration is a JSON object mapping **action IDs** (camelCase) to **key strings** (or arrays of strings).

```json
{
  "cursorUp": ["up", "ctrl+p"],
  "cursorDown": ["down", "ctrl+n"],
  "submit": "enter",
  "newLine": ["shift+enter", "ctrl+enter"]
}
```

Use an empty array to remove a default binding entirely:

```json
{
  "cursorUp": []
}
```

### Key Syntax

Keys are specified as `modifier+key`.

- **Modifiers**: `ctrl`, `alt`, `shift` (and combinations like `ctrl+shift`).
- **Keys**:
  - Letters: `a`, `b`, `c`...
  - Numbers: `1`, `2`...
  - Function keys: `f1`–`f20`
  - Special keys: `enter`, `escape`, `tab`, `space`, `backspace`, `delete`, `insert`, `clear`,
    `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`
  - Symbols: single-character keys like `` ` ``, `-`, `=`, `[`, `]`, `\`, `;`, `'`, `,`, `.`, `/`,
    and their shifted variants (`!`, `@`, `#`, `$`, `%`, `^`, `&`, `*`, `(`, `)`, `_`, `+`, `{`, `}`,
    `|`, `:`, `"`, `<`, `>`, `?`)

**Synonyms**:
- `return` -> `enter`
- `esc` -> `escape`

## Actions & Defaults

### Cursor Movement

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `cursorUp` | `up` | Move cursor up |
| `cursorDown` | `down` | Move cursor down |
| `cursorLeft` | `left`, `ctrl+b` | Move cursor left |
| `cursorRight` | `right`, `ctrl+f` | Move cursor right |
| `cursorWordLeft` | `alt+left`, `ctrl+left`, `alt+b` | Move cursor word left |
| `cursorWordRight` | `alt+right`, `ctrl+right`, `alt+f` | Move cursor word right |
| `cursorLineStart` | `home`, `ctrl+a` | Move to line start |
| `cursorLineEnd` | `end`, `ctrl+e` | Move to line end |
| `jumpForward` | `ctrl+]` | Jump forward to character |
| `jumpBackward` | `ctrl+alt+]` | Jump backward to character |
| `pageUp` | `pageup` | Scroll up by page |
| `pageDown` | `pagedown` | Scroll down by page |

### Deletion

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `deleteCharBackward` | `backspace` | Delete character backward |
| `deleteCharForward` | `delete`, `ctrl+d` | Delete character forward |
| `deleteWordBackward` | `ctrl+w`, `alt+backspace` | Delete word backward |
| `deleteWordForward` | `alt+d`, `alt+delete` | Delete word forward |
| `deleteToLineStart` | `ctrl+u` | Delete to line start |
| `deleteToLineEnd` | `ctrl+k` | Delete to line end |

### Text Input

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `newLine` | `shift+enter`, `ctrl+enter` | Insert new line |
| `submit` | `enter` | Submit input |
| `tab` | `tab` | Tab / autocomplete |

### Application

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `interrupt` | `escape` | Cancel / abort |
| `clear` | `ctrl+c` | Clear editor (or cancel selection) |
| `exit` | `ctrl+d` | Exit (when editor empty) |
| `suspend` | `ctrl+z` | Suspend to background |
| `externalEditor` | `ctrl+g` | Open in external editor |

### Clipboard & Kill Ring

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `copy` | `ctrl+c` | Copy selection |
| `pasteImage` | `ctrl+v` | Paste image from clipboard |
| `yank` | *(none)* | Paste most recently deleted text (reserved) |
| `yankPop` | *(none)* | Cycle through deleted text (reserved) |
| `undo` | `ctrl+-` | Undo last edit |

### Models & Thinking

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `selectModel` | `ctrl+l` | Open model selector |
| `cycleModelForward` | `ctrl+p` | Cycle to next model |
| `cycleModelBackward` | `ctrl+shift+p` | Cycle to previous model |
| `cycleThinkingLevel` | `shift+tab` | Cycle thinking level |

### Display & Tools

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `expandTools` | `ctrl+o` | Collapse/expand tool output |
| `toggleThinking` | `ctrl+t` | Collapse/expand thinking blocks |

### Session

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `newSession` | - | Start a new session |
| `tree` | - | Open session tree navigator |
| `fork` | - | Fork current session |

### Message Queue

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `followUp` | `alt+enter` | Queue follow-up message |
| `dequeue` | `alt+up` | Restore queued messages to editor |

### Selection (Lists/Pickers)

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `selectUp` | `up` | Move selection up |
| `selectDown` | `down` | Move selection down |
| `selectPageUp` | `pageup` | Page up in list |
| `selectPageDown` | `pagedown` | Page down in list |
| `selectConfirm` | `enter` | Confirm selection |
| `selectCancel` | `escape`, `ctrl+c` | Cancel selection |

### Session Picker

| Action ID | Default Keys | Description |
|-----------|--------------|-------------|
| `deleteSession` | `ctrl+d` | Delete session |
| `toggleSessionPath` | `ctrl+p` | Toggle path display |
| `toggleSessionSort` | `ctrl+s` | Toggle sort mode |
| `toggleSessionNamedFilter` | `ctrl+n` | Toggle named-only filter |
| `renameSession` | *(none)* | Rename session (reserved) |
| `deleteSessionNoninvasive` | *(none)* | Delete session when query is empty (reserved) |

## Context-dependent conflicts

Some keys are intentionally bound to multiple actions and are resolved based on UI state:

- `ctrl+c` can mean **Copy** (selection), **Clear** (editor), or **abort** (when running).
- `ctrl+d` is **DeleteCharForward** in the editor, **Exit** when the editor is empty, and
  **DeleteSession** inside the session picker.

