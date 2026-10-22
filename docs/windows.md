# Windows Notes

Pi runs natively on Windows, but there are some platform-specific differences to be aware of.

## Shell Requirements (bash tool)

Pi’s `bash` tool (and `!command` shortcuts) require a **POSIX shell**. On Windows, install
one of:

1. Git Bash (recommended): `C:\Program Files\Git\bin\bash.exe`
2. MSYS2/Cygwin bash on `PATH`
3. WSL bash (if exposed on `PATH`)

When `shell_path` is not set, Pi looks for a bash in this order:

1. Git for Windows in its standard locations: `%ProgramFiles%\Git\bin\bash.exe`
   (and the x86 / per-user `%LOCALAPPDATA%\Programs\Git` installs).
2. `PATH`, in order: a `bash.exe` (Git Bash, MSYS2, Cygwin, Scoop), or Git's
   `bin\bash.exe` next to a `git.exe` on `PATH` (Git's default installer option
   only adds `Git\cmd`).
3. WSL's launcher, `C:\Windows\System32\bash.exe`, as a last resort. It works,
   but commands then run inside your Linux distro, where Windows paths appear
   under `/mnt/c/...` and Windows tools are not on the Linux `PATH`. A UNC
   working directory (`\\server\share`) may not translate there. Pi prints a
   one-time warning (stderr, or the TUI log) the first time it falls back to it.

If none is found, the `bash` tool fails with a message listing these options. You can also set a custom shell in settings:

```json
{
  "shell_path": "C:\\Program Files\\Git\\bin\\bash.exe"
}
```

## npm Packages

npm-sourced packages (`pi install npm:...`, or npm entries under `packages` in
`settings.json`) run `npm`. Pi looks it up on `PATH` the way the shell does:
directories in `PATH` order, and in each one the `%PATHEXT%` extensions that
Windows can start (`.com`, `.exe`, `.bat`, `.cmd`). That finds the Node.js
installer's `npm.cmd` as well as an `npm.exe` shim such as pnpm's. Empty and
relative `PATH` entries are skipped. If no `npm` is found, loading the
configured packages fails with `Failed to spawn npm: program not found`.

## Desktop Automation (`computer` tool)

The `computer` tool has no Windows backend yet. With `computer.enable_computer`
set, every action on Windows fails with `native computer backend is not
implemented for this operating system`, so leave the setting off there. The
native backends are macOS and Linux on X11; see the Computer section of
[settings.md](settings.md#computer-desktop-automation).

## Keybindings

### Windows Terminal

- **Newline**: Use `Ctrl+Enter` to insert a newline in the editor (instead of `Shift+Enter` which is common on Linux/macOS). `Enter` submits the message.

## Clipboard

Pi attempts to use the system clipboard for `/copy` and image pasting.

- Ensure you are running in a terminal that supports clipboard access if using remote sessions (e.g. via SSH).
- If clipboard operations fail, Pi will typically fall back to printing the content or ignoring the paste.
- **WSL**: the Linux `pi` binary has no X11/Wayland display inside WSL (unless WSLg is running), so Pi detects WSL and uses Windows' `clip.exe` for `/copy` and `/share`, and `powershell.exe` for image paste. Both are on `PATH` in a default WSL setup; no extra configuration is needed.

## Paths

- Pi supports both forward slashes `/` and backslashes `\` in paths.
- When configuring paths in JSON (e.g. `settings.json`), remember to escape backslashes: `C:\Users\Name\.pi`.
- Use forward slashes in `settings.json` for cross-platform compatibility if possible (`C:/Users/Name/.pi`).

Extensions importing `node:path` receive Windows path behavior on Windows,
including drive letters, UNC shares, `sep = "\\"`, and `delimiter = ";"`.
`path.win32` and `path.posix` are also available explicitly on every host.
Relative paths resolve against the extension runtime's working directory.
The runtime reports `process.platform` as `win32` on Windows even when access
to environment variables is disabled.

The extension filesystem shim retains UNC share roots and extended-length
Windows path prefixes through reads, metadata queries, and directory listings.
Path normalization stops at the drive or share root, and native access checks
receive the same path representation as the filesystem operation.

## Shell Commands

- The `bash` tool and `!command` shortcuts use the bash chosen above (see "Shell Requirements").
- `shell_path` must point to a POSIX shell such as `bash.exe`: commands are run with `-c` and wrapped in POSIX shell syntax, so `cmd.exe`, `powershell.exe` and `pwsh.exe` do not work there.
- Secret resolution in `models.json` uses `cmd /C` to execute `!commands`.
