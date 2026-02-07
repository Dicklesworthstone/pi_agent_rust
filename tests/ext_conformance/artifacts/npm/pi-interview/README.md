<p>
  <img src="https://raw.githubusercontent.com/nicobailon/pi-interview-tool/main/banner.png" alt="pi-interview" width="1100">
</p>

# Interview Tool

A custom tool for pi-agent that opens a web-based form to gather user responses to clarification questions.

https://github.com/user-attachments/assets/52285bd9-956e-4020-aca5-9fbd82916934

## Installation

```bash
pi install npm:pi-interview
```

Restart pi to load the extension.

**Requirements:**
- pi-agent v0.35.0 or later (extensions API)

## Features

- **Question Types**: Single-select, multi-select, text input, and image upload
- **"Other" Option**: Single/multi select questions support custom text input
- **Per-Question Attachments**: Attach images to any question via button, paste, or drag & drop
- **Keyboard Navigation**: Full keyboard support with arrow keys, Tab, Enter
- **Auto-save**: Responses saved to localStorage, restored on reload
- **Session Timeout**: Configurable timeout with countdown badge, refreshes on activity
- **Multi-Agent Support**: Queue detection prevents focus stealing when multiple agents run interviews
- **Queue Toast Switcher**: Active interviews show a top-right toast with a dropdown to open queued sessions
- **Session Recovery**: Abandoned/timed-out interviews save questions for later retry
- **Save Snapshots**: Save interview state to HTML for later review or revival
- **Session Status Bar**: Shows project path, git branch, and session ID for identification
- **Image Support**: Drag & drop anywhere on question, file picker, paste image or path
- **Path Normalization**: Handles shell-escaped paths (`\ `) and macOS screenshot filenames (narrow no-break space before AM/PM)
- **Themes**: Built-in default + optional light/dark + custom theme CSS

## How It Works

```
┌─────────┐      ┌──────────────────────────────────────────┐      ┌─────────┐
│  Agent  │      │              Browser Form                │      │  Agent  │
│ invokes ├─────►│                                          ├─────►│receives │
│interview│      │  answer → answer → attach img → answer   │      │responses│
└─────────┘      │     ↑                                    │      └─────────┘
                 │     └── auto-save, timeout resets ───────┤
                 └──────────────────────────────────────────┘
```

**Lifecycle:**
1. Agent calls `interview()` → local server starts → browser opens form
2. User answers at their own pace; each change auto-saves and resets the timeout
3. Session ends via:
   - **Submit** (`⌘+Enter`) → responses returned to agent
   - **Timeout** → warning overlay, option to stay or close
   - **Escape × 2** → quick cancel
4. Window closes automatically; agent receives responses (or `null` if cancelled)

**Timeout behavior:** The countdown (visible in corner) resets on any activity - typing, clicking, or mouse movement. When it expires, an overlay appears giving the user a chance to continue. Progress is never lost thanks to localStorage auto-save.

**Multi-agent behavior:** When multiple agents run interviews simultaneously, only the first auto-opens the browser. Subsequent interviews are queued and shown as a URL in the tool output, preventing focus stealing. Active interviews also surface a top-right toast with a dropdown to open queued sessions. A session status bar at the top of each form shows the project path, git branch, and session ID for easy identification.

## Usage

The interview tool is invoked by pi-agent, not imported directly:

```javascript
// Create a questions JSON file, then call the tool
await interview({
  questions: '/path/to/questions.json',
  timeout: 600,  // optional, seconds (default: 600)
  verbose: false // optional, debug logging
});
```

## Question Schema

```json
{
  "title": "Project Setup",
  "description": "Optional description text",
  "questions": [
    {
      "id": "framework",
      "type": "single",
      "question": "Which framework?",
      "options": ["React", "Vue", "Svelte"],
      "recommended": "React"
    },
    {
      "id": "features",
      "type": "multi",
      "question": "Which features?",
      "context": "Select all that apply",
      "options": ["Auth", "Database", "API"],
      "recommended": ["Auth", "Database"]
    },
    {
      "id": "notes",
      "type": "text",
      "question": "Additional requirements?"
    },
    {
      "id": "mockup",
      "type": "image",
      "question": "Upload a design mockup"
    }
  ]
}
```

### Question Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique identifier |
| `type` | string | `single`, `multi`, `text`, or `image` |
| `question` | string | Question text |
| `options` | string[] or object[] | Choices (required for single/multi). Can be strings or `{ label, code? }` objects |
| `recommended` | string or string[] | Highlighted option(s) with `*` indicator |
| `context` | string | Help text shown below question |
| `codeBlock` | object | Code block displayed below question text |

### Code Blocks

Questions and options can include code blocks for displaying code snippets, diffs, and file references.

**Question-level code block** (displayed above options):
```json
{
  "id": "review",
  "type": "single",
  "question": "Review this implementation",
  "codeBlock": {
    "code": "function add(a, b) {\n  return a + b;\n}",
    "lang": "ts",
    "file": "src/math.ts",
    "lines": "10-12",
    "highlights": [2]
  },
  "options": ["Approve", "Request changes"]
}
```

**Options with code blocks**:
```json
{
  "options": [
    {
      "label": "Use async/await",
      "code": { "code": "const data = await fetch(url);", "lang": "ts" }
    },
    {
      "label": "Use promises",
      "code": { "code": "fetch(url).then(data => ...);", "lang": "ts" }
    },
    "Keep current implementation"
  ]
}
```

**Diff display** (set `lang: "diff"`):
```json
{
  "codeBlock": {
    "code": "--- a/file.ts\n+++ b/file.ts\n@@ -1,3 +1,4 @@\n const x = 1;\n+const y = 2;\n const z = 3;",
    "lang": "diff",
    "file": "src/file.ts"
  }
}
```

| CodeBlock Field | Type | Description |
|-----------------|------|-------------|
| `code` | string | The code content (required) |
| `lang` | string | Language for display (e.g., "ts", "diff") |
| `file` | string | File path to display in header |
| `lines` | string | Line range to display (e.g., "10-25") |
| `highlights` | number[] | Line numbers to highlight |
| `title` | string | Optional title above code |

Line numbers are shown when `file` or `lines` is specified. Diff syntax (`+`/`-` lines) is automatically styled when `lang` is "diff".

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `↑` `↓` | Navigate options |
| `←` `→` | Navigate between questions |
| `Tab` | Cycle through options |
| `Enter` / `Space` | Select option |
| `⌘+V` | Paste image or file path |
| `⌘+Enter` | Submit form |
| `Esc` | Show exit overlay (press twice to quit) |
| `⌘+Shift+L` | Toggle theme (if enabled; appears in shortcuts bar) |

## Configuration

Settings in `~/.pi/agent/settings.json`:

```json
{
  "interview": {
    "timeout": 600,
    "port": 19847,
    "snapshotDir": "~/.pi/interview-snapshots/",
    "autoSaveOnSubmit": true,
    "theme": {
      "mode": "auto",
      "name": "default",
      "lightPath": "/path/to/light.css",
      "darkPath": "/path/to/dark.css",
      "toggleHotkey": "mod+shift+l"
    }
  }
}
```

**Timeout precedence**: params > settings > default (600s)

**Snapshot settings:**
- `snapshotDir`: Directory for saved interview snapshots (default: `~/.pi/interview-snapshots/`)
- `autoSaveOnSubmit`: Automatically save snapshot on successful submit (default: `true`)

**Port setting**: Set a fixed `port` (e.g., `19847`) to use a consistent port across sessions.

**Theme notes:**
- `mode`: `dark` (default), `light`, or `auto` (follows OS unless overridden)
- `name`: built-in themes are `default` and `tufte`
- `lightPath` / `darkPath`: optional CSS file paths (absolute or relative to cwd)
- `toggleHotkey`: optional; when set, toggles light/dark and persists per browser profile

## Theming

The interview form supports light/dark themes with automatic OS detection and user override.

### Built-in Themes

| Theme | Description |
|-------|-------------|
| `default` | Monospace, IDE-inspired aesthetic |
| `tufte` | Serif fonts (Cormorant Garamond), book-like feel |

### Theme Modes

- **`dark`** (default): Dark background, light text
- **`light`**: Light background, dark text  
- **`auto`**: Follows OS preference, user can toggle and override persists in localStorage

### Custom Themes

Create custom CSS files that override the default variables:

```css
:root {
  --bg-body: #f8f8f8;
  --bg-card: #ffffff;
  --bg-elevated: #f0f0f0;
  --bg-selected: #d0d0e0;
  --bg-hover: #e8e8e8;
  --fg: #1a1a1a;
  --fg-muted: #6c6c6c;
  --fg-dim: #8a8a8a;
  --accent: #5f8787;
  --accent-hover: #4a7272;
  --accent-muted: rgba(95, 135, 135, 0.15);
  --border: #5f87af;
  --border-muted: #b0b0b0;
  --border-focus: #8a8a9a;
  --border-active: #9090a0;
  --success: #87af87;
  --warning: #d7af5f;
  --error: #af5f5f;
  --focus-ring: rgba(95, 135, 175, 0.2);
}
```

Then reference in settings or params:

```json
{
  "interview": {
    "theme": {
      "mode": "auto",
      "lightPath": "~/my-themes/light.css",
      "darkPath": "~/my-themes/dark.css",
      "toggleHotkey": "mod+shift+l"
    }
  }
}
```

### Toggle Hotkey

When `toggleHotkey` is set (e.g., `"mod+shift+l"`), users can switch between light/dark modes. The preference persists in the browser's localStorage across sessions.

## Response Format

```typescript
interface Response {
  id: string;
  value: string | string[];
  attachments?: string[];  // image paths attached to non-image questions
}
```

Example:
```
- framework: React [attachments: /path/to/diagram.png]
- features: Auth, Database
- notes: Need SSO support
- mockup: /tmp/uploaded-image.png
```

## File Structure

```
interview/
├── index.ts       # Tool entry point, parameter schema
├── settings.ts    # Shared settings module
├── server.ts      # HTTP server, request handling
├── schema.ts      # TypeScript interfaces for questions/responses
└── form/
    ├── index.html # Form template
    ├── styles.css # Base styles (dark tokens)
    ├── themes/    # Theme overrides (light/dark)
    └── script.js  # Form logic, keyboard nav, image handling
```

## Session Recovery

If an interview times out or is abandoned (tab closed, lost connection), the questions are automatically saved to `~/.pi/interview-recovery/` for later retry.

**Recovery files:**
- Location: `~/.pi/interview-recovery/`
- Format: `{date}_{time}_{project}_{branch}_{sessionId}.json`
- Example: `2026-01-02_093000_myproject_main_65bec3f4.json`
- Auto-cleanup: Files older than 7 days are deleted

**To retry an abandoned interview:**
```javascript
interview({ questions: "~/.pi/interview-recovery/2026-01-02_093000_myproject_main_65bec3f4.json" })
```

## Saving Interviews

Save a snapshot of your interview at any time for later review or to resume.

**Manual Save:**
- Click the Save button (header or footer)
- Saves to `~/.pi/interview-snapshots/` by default
- Creates folder with `index.html` + `images/` subfolder

**Auto-save on Submit:**
- Enabled by default (`autoSaveOnSubmit: true` in settings)
- Automatically saves after successful submission
- Folder name includes `-submitted` suffix

**Reviving a Saved Interview:**
```javascript
interview({ questions: "~/.pi/interview-snapshots/project-setup-myapp-main-2026-01-20-141523/index.html" })
```
The form opens with answers pre-populated. Edit and submit as normal.

**Configuration:**
```json
{
  "interview": {
    "snapshotDir": "~/.pi/interview-snapshots/",
    "autoSaveOnSubmit": true
  }
}
```

**Snapshot Structure:**
```
~/.pi/interview-snapshots/
  {title}-{project}-{branch}-{timestamp}[-submitted]/
    index.html          # Human-readable + embedded JSON for revival
    images/
      mockup.png        # Uploaded images (relative paths in HTML)
```

## Limits

- Max 12 images total per submission
- Max 5MB per image
- Max 4096x4096 pixels per image
- Allowed types: PNG, JPG, GIF, WebP
