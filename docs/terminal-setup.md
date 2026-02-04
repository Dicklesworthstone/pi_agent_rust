# Terminal Setup

Pi works in any modern terminal, but some features (like image display) and key combos require
terminal-specific support or configuration.

## Recommended Terminals

- **Ghostty**: Excellent performance and Kitty graphics support.
- **WezTerm**: Great cross-platform support and iTerm graphics protocol.
- **iTerm2**: Solid iTerm graphics protocol support (macOS).
- **Kitty**: Best-in-class Kitty graphics support.
- **Windows Terminal**: Good Unicode support, limited inline image support.

## Keyboard Protocol Notes

Some terminals need **Kitty keyboard protocol** enabled for reliable modifier combos
(e.g., `Shift+Enter`, `Alt+Backspace`).

### Ghostty

Add to `~/.config/ghostty/config`:

```
keybind = alt+backspace=text:\x1b\x7f
keybind = shift+enter=text:\n
```

### WezTerm

Create `~/.wezterm.lua`:

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.enable_kitty_keyboard = true
return config
```

### VS Code (Integrated Terminal)

Add to `keybindings.json`:

```json
{
  "key": "shift+enter",
  "command": "workbench.action.terminal.sendSequence",
  "args": { "text": "\u001b[13;2u" },
  "when": "terminalFocus"
}
```

### Windows Terminal

Add to `settings.json`:

```json
{
  "actions": [
    {
      "command": { "action": "sendInput", "input": "\u001b[13;2u" },
      "keys": "shift+enter"
    }
  ]
}
```

### IntelliJ IDEA (Integrated Terminal)

IntelliJ’s terminal can’t distinguish `Shift+Enter` from `Enter`. For the best experience,
use an external terminal.

If you want the hardware cursor visible, set `PI_HARDWARE_CURSOR=1` before running `pi`.

## Image Support

Pi detects terminal capabilities to display images inline (e.g., when using the `read` tool on an
image file). If the terminal does not support images, Pi shows a placeholder like
`[Image: 1024x768 placeholder]`.

To block images entirely, set:

```json
{
  "images": {
    "block_images": true
  }
}
```

There is also a planned terminal toggle:

```json
{
  "terminal": {
    "show_images": false
  }
}
```

`terminal.show_images` and `terminal.clear_on_shrink` are tracked by `bd-xk3g` and not wired yet.

## Keybindings

Some terminals intercept key combinations needed by Pi (e.g., `Ctrl+Arrow`, `Shift+Enter`).

- **Windows Terminal**: Use `Ctrl+Enter` for newlines if `Shift+Enter` isn’t available.
- **VS Code Terminal**: Some shortcuts may be captured by VS Code. Check your
  `terminal.integrated.commandsToSkipShell` setting.
