# Computer Automation Tool: `computer`

> **Tool family:** OS & Desktop Automation (Opt-in)  
> **Bead ID:** `bd-cv653.2.5`  
> **Module:** `src/computer.rs`

---

## 1. Overview

The `computer` tool provides desktop automation, accessibility tree inspection, display/window enumeration, and input synthesis for graphical environments.

---

## 2. Supported Actions

- `list_displays`: Enumerates active physical displays with resolution, scale factor, and primary display flag.
- `list_windows`: Lists open application windows, bounding boxes, titles, and active focus state.
- `screenshot`: Captures full screen or target window into a PNG artifact.
- `mouse_move`, `mouse_click`, `mouse_drag`: Simulates precise cursor movement and clicks.
- `key_type`, `key_press`: Synthesizes keyboard typing and key combinations (e.g. `ctrl+c`, `cmd+s`).
- `ax_tree`: Dumps the OS accessibility hierarchy for UI element inspection.
- `clipboard_read`, `clipboard_write`: Interacts with the system clipboard.

---

## 3. Configuration & Safety

```toml
[computer]
enable_computer = true
require_approval = true
screenshot_dir = ".pi/screenshots"
```

All mutating actions (`mouse_click`, `key_type`, `clipboard_write`) declare write effects and require explicit operator confirmation when approval mode is active. An in-memory structured audit log (`ComputerAuditEntry`) records all actions.
