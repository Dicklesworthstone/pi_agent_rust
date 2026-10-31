# Issue #239: ConHost input ownership investigation

## Status

Production FTUI now acquires the native console-input lease from
`console_input.rs`, releases it for the external editor, and restores the
original input mode during shutdown. This closes the previously unwired
native mouse/resize input path. It is **not a verified fix for abaynp's
report**: do not close #239 on the basis of this change.

DSR remains unavailable in the authoring environment:
`dsr quality --tool pi_agent_rust` returned command-not-found (exit 127).
No compiler, test, Windows, or real-ConHost execution result is claimed.

The initial investigation reviewed pi commit
`c7a2c90887ff8ed7281a064b31ac6570128f24ff`, which used FTUI 0.7.0. The
production integration was reviewed against `26142ee` and its locked FTUI
0.9.0 and Crossterm 0.29.0 sources. The published FTUI crate checksums were
verified against `Cargo.lock`; the native input-mode gap still exists there.

## Concrete gap, not yet a reproduced trigger

Before this integration, Pi's `release_terminal`/`reacquire_terminal` changed
raw mode and emitted ANSI mouse sequences. FTUI's terminal session similarly
writes ANSI mouse sequences. Crossterm's Windows raw-mode implementation clears
only line, echo and processed-input bits. It does not clear QuickEdit or
establish native mouse and resize input flags. By contrast, Crossterm's native
`EnableMouseCapture` implementation clears QuickEdit and sets mouse, window
and extended flags. Production now owns those native input flags explicitly
through the scoped lease.

This establishes a concrete ConHost selection/input-mode difference, but it
does not establish that selection caused the reported blank screen or that
all ConHost stalls have the same cause. Another diagnostic discriminator is
`ENABLE_PROCESSED_INPUT`: when it is enabled, Windows handles Ctrl+C instead of
placing it in the input buffer. A live UI with cooked/processed input would
point toward a failed terminal reacquisition rather than a normal keybinding.

Relevant primary sources:

- [Windows SetConsoleMode](https://learn.microsoft.com/en-us/windows/console/setconsolemode)
- [Crossterm 0.29.0 Windows terminal modes](https://docs.rs/crate/crossterm/0.29.0/source/src/terminal/sys/windows.rs)
- [Crossterm 0.29.0 native mouse capture](https://docs.rs/crate/crossterm/0.29.0/source/src/event/sys/windows.rs)
- [FTUI 0.9.0 terminal lifecycle](https://docs.rs/crate/ftui-core/0.9.0/source/src/terminal_session.rs)

The current Pi handler already filters releases, gates SIGTSTP suspension to
Unix, and runs the external editor synchronously on the event-loop thread.
FTUI 0.9.0 also checks Windows input readiness immediately before reading,
and Pi's frame budget avoids dropping every slow frame before presentation.
These existing protections were not reapplied as an explanation for #239.
The in-model watchdog measures completed callbacks; it cannot, by itself,
report a callback that never returns or a runtime I/O operation outside a
callback that stops progress.

## Probe

Use the `ftui_conhost_probe` example built by an appropriate DSR lane. From the
same cmd/ConHost window, run these separately with new trace filenames:

```text
ftui_conhost_probe.exe baseline.jsonl
ftui_conhost_probe.exe native.jsonl --native-input
```

Repeat both with `--fullscreen` to compare alternate-screen and inline modes.
The default probe uses inline mode. Keep the console's QuickEdit setting the
same between the baseline and comparison runs. Type, resize, and click/drag
in the console. `q` quits. `s` synchronously cycles the native input portion of
an editor handoff; it does not launch an editor or claim to exercise a full
external-editor lifecycle. The probe has no provider, agent or Pi session.

A separate observer writes and flushes JSONL every 500 ms. The model requests
FTUI timer ticks every 250 ms. The observer never calls poll/read on terminal
input and never writes to stdout/stderr. Records contain counters, mode bits,
callback age and phase, not key values, typed text, prompts or credentials.
Existing trace files are never overwritten. The native-input flag enables
the same scoped mode lease used by production and is rejected on non-Windows
hosts; the baseline intentionally leaves native input modes alone. A
trace-write failure asks the model to quit on its next timer tick; the
observer's I/O error is then returned instead of silently losing evidence.

Interpretation requires the accompanying reproduction observations:

- Increasing callback age with stalled tick/view counts and `outside-model`
  narrows the stall to outside model callbacks. It does **not** distinguish
  native input polling, output writes, or command delivery by itself.
- Advancing ticks with no new key events while typing points toward input
  delivery, not a completely stopped model loop.
- `quick_edit: true` is a precondition to investigate, not proof of active
  selection. The probe does not query console selection state.
- `processed_input`, `line_input` or `echo_input` during the active loop help
  identify a lost raw-mode transition. The initial/final records are expected
  to show shell modes. Sampling can straddle startup/shutdown transitions;
  a single cooked-mode record near either boundary is not proof of a stall.
- View counts mean model rendering completed, not that terminal output was
  flushed. A stopped observer or a mode-query error is not a passing result.

## Native lease and tests

`src/interactive_ftui/console_input.rs` opens `CONIN$` with read/write access,
uses the existing safe `winapi-util` dependency, preserves one original mode
across handoffs, reapplies native flags on resume, and restores the original
mode before restart/exit. It honors mouse-capture opt-out. Its two-step restore
handles the documented requirement to set `ENABLE_EXTENDED_FLAGS` while
changing QuickEdit. Failed mode writes/readbacks are errors, not silent success.

`tests/ftui_conhost_console.rs` includes portable policy tests and one explicitly
ignored, exclusive-real-console Windows test. The portable tests model extended
mode semantics, cover all 1,024 low-bit combinations, verify preservation of
unowned bits, and check that repeated editor mode changes do not overwrite the
original baseline. They do not simulate a real ConHost selection stall.

The production module now also compiles those portable lease tests. Additional
in-module regressions cover raw/native/output ordering, failed reacquisition,
recovery-file persistence, fatal-error propagation, and cancellation of active
turns and handoffs before quitting. Startup regressions also keep real controlled
turns alive across failed lease acquisition and runtime exit, cover publication
on either side of shutdown, and verify that a queued initial prompt cannot reach
the provider after the runtime closes. A second explicitly ignored real-console
test calls the production runtime wrapper, checks mouse/resize modes for both
capture settings, and verifies restoration after an app failure.

Native tests require a DSR Windows lane with exclusive console access; they
must not be unignored on a redirected or parallel test runner. Compiler/test
validation and the native comparison remain outstanding. Use DSR, not Cargo
or GitHub Actions as an alternate quality route.

## Production lifecycle

`interactive_ftui.rs` declares `mod console_input`. Its runtime wrapper acquires
the lease after the log redirect guard and before `App::run`, and finishes it
before dropping the log guard, joining the driver, or calling `/restart`.
Acquisition failure explicitly drops the unrun App before returning. Model
teardown aborts its admitted handoff and driver work. The runtime also retains
cancellation authority outside App and closes the control lane on every exit,
including success: a mutex-protected shutdown fence aborts any initial turn
published concurrently with teardown before that turn reaches its first poll.
The driver's join therefore does not rely on a channel disconnect alone while
an initial prompt is already running.

`release_terminal` calls native `suspend()` after disabling raw mode, including
when raw-mode release fails. `reacquire_terminal` calls native `resume()` after
enabling raw mode and before output. An ordinary editor failure with a restored
terminal stays recoverable in the composer. Failed terminal reacquisition saves
the edited result, or the original draft when editing failed, in a securely
created recovery file that survives temporary ownership. Errors report only
the recovery path, and only after persistence succeeds; save failures are
reported explicitly without exposing draft text.

A fatal reacquisition failure cancels the admitted handoff, shared driver
abort handle, and live controlled turn before requesting normal runtime quit.
The driver can then finish cancellation and observe its closed command channel
instead of waiting for the provider or tools to finish naturally. The terminal
failure reaches the final exit result after cleanup, without resuming input
polling on a potentially cooked console. These lifecycle changes are not
evidence that the original #239 report has been reproduced or resolved.
