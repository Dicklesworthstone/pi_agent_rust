# Windows Notes

Pi runs natively on Windows, but there are some platform-specific differences to be aware of.

## Shell Requirements (bash tool)

Pi’s `bash` tool (and `!command` shortcuts) require a **POSIX shell**. On Windows, install
one of:

1. Git Bash (recommended): `C:\Program Files\Git\bin\bash.exe`
2. MSYS2/Cygwin bash on `PATH`
3. WSL bash (if exposed on `PATH`)

You can also set a custom shell in settings:

```json
{
  "shell_path": "C:\\Program Files\\Git\\bin\\bash.exe"
}
```

## Keybindings

### Windows Terminal

- **Newline**: Use `Ctrl+Enter` to insert a newline in the editor (instead of `Shift+Enter` which is common on Linux/macOS). `Enter` submits the message.

## Clipboard

Pi attempts to use the system clipboard for `/copy` and image pasting.

- Ensure you are running in a terminal that supports clipboard access if using remote sessions (e.g. via SSH).
- If clipboard operations fail, Pi will typically fall back to printing the content or ignoring the paste.
- **WSL**: the Linux `pi` binary has no X11/Wayland display inside WSL (unless WSLg is active), so the usual clipboard backend can't open a clipboard there. Pi detects WSL and bridges `/copy` and image pasting through the Windows interop binaries (`clip.exe`, `powershell.exe`) that WSL already puts on `PATH`, so no extra setup is needed.

## Paths

- Pi supports both forward slashes `/` and backslashes `\` in paths.
- When configuring paths in JSON (e.g. `settings.json`), remember to escape backslashes: `C:\Users\Name\.pi`.
- Use forward slashes in `settings.json` for cross-platform compatibility if possible (`C:/Users/Name/.pi`).

## Shell Commands

- In `bash` tools and `!command` shortcuts, Pi tries to use `sh` (Git Bash or similar) if available.
- If configuring `shell_path`, point it to your preferred shell executable (e.g., `bash.exe`, `powershell.exe`, `pwsh.exe`).
- Secret resolution in `models.json` uses `cmd /C` to execute `!commands`.
