# Themes

Pi supports JSON-based themes to customize the terminal UI colors.

## Usage

Use `/theme` to switch themes interactively, or set the `theme` option in `settings.json`.

```bash
# In interactive mode
/theme ocean-dark
```

Theme selection is also exposed via `/settings` (choose the **Theme** entry).

```json
// settings.json
{
  "theme": "ocean-dark"
}
```

## Discovery

Pi searches for themes in the following locations:

1. **Global**: `~/.pi/agent/themes/*.json`
2. **Project**: `.pi/themes/*.json`

## File Format

A theme is a JSON file defining colors for UI elements and syntax highlighting. All colors must be 6-digit hex codes (`#RRGGBB`).

### Schema

```json
{
  "name": "ocean-dark",
  "version": "1.0",
  "colors": {
    "foreground": "#d4d4d4",
    "background": "#1e1e1e",
    "accent": "#007acc",
    "success": "#4ec9b0",
    "warning": "#ce9178",
    "error": "#f44747",
    "muted": "#6a6a6a"
  },
  "syntax": {
    "keyword": "#569cd6",
    "string": "#ce9178",
    "number": "#b5cea8",
    "comment": "#6a9955",
    "function": "#dcdcaa"
  },
  "ui": {
    "border": "#3c3c3c",
    "selection": "#264f78",
    "cursor": "#aeafad"
  }
}
```

### Fields

- **name**: Display name (used in `/theme` command).
- **colors**:
  - `foreground`: Primary text color.
  - `background`: Terminal background color.
  - `accent`: User input, links, and highlights.
  - `success`: Success messages.
  - `warning`: Warnings.
  - `error`: Error messages.
  - `muted`: Thinking blocks, secondary text.
- **syntax**: Used for Markdown code blocks.
- **ui**:
  - `border`: Panel borders.
  - `selection`: Selected item background in lists/pickers.
  - `cursor`: Editor cursor color.

## Built-in Themes

Pi includes built-in themes:
- `dark` (default)
- `light`
- `solarized`
