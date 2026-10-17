//! Native Windows console-input ownership for the FTUI lifecycle.
//!
//! ANSI mouse reporting does not disable `ConHost` `QuickEdit`. Crossterm raw
//! mode also leaves `QuickEdit`, mouse-input and window-input flags alone.
//! Keep one original mode across release/reacquire; never snapshot a child's
//! mode as the shell's baseline. No terminal input is read by this module.

use std::io;
use std::marker::PhantomData;
use std::rc::Rc;

#[cfg(any(windows, test))]
const WINDOW_INPUT: u32 = 0x0008;
#[cfg(any(windows, test))]
const MOUSE_INPUT: u32 = 0x0010;
#[cfg(any(windows, test))]
const QUICK_EDIT: u32 = 0x0040;
#[cfg(any(windows, test))]
const EXTENDED_FLAGS: u32 = 0x0080;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug)]
struct ModeLease {
    original: u32,
    mouse: bool,
}

#[cfg(any(windows, test))]
impl ModeLease {
    const fn capture(self, current: u32) -> u32 {
        let current = (current | WINDOW_INPUT) & !MOUSE_INPUT;
        if self.mouse {
            (current | MOUSE_INPUT | EXTENDED_FLAGS) & !QUICK_EDIT
        } else {
            // Preserve native selection, but do not deliver native mouse
            // records to an app whose user explicitly opted out of capture.
            current
        }
    }

    const fn restore_steps(self) -> [u32; 2] {
        // Without EXTENDED_FLAGS, SetConsoleMode ignores QuickEdit changes.
        // The second write restores a baseline that did not include that bit.
        [self.original | EXTENDED_FLAGS, self.original]
    }
}

/// A same-thread lease, held outside `App::run` but inside its cleanup scope.
/// The guard must be dropped before any `process::exit`/restart path.
#[must_use]
pub struct Guard {
    active: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Guard {
    /// Restore explicitly so normal shutdown can report a restoration error.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        if self.active {
            restore()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = restore() {
                tracing::error!(target: "pi::ftui::console", %error,
                    "failed to restore the original console input mode");
            }
            self.active = false;
        }
    }
}

/// Run an app under the native-input lease, including failed startup paths.
/// The app owns channels the driver waits on, so it must be dropped before
/// returning an acquisition error to a caller that will join that driver.
pub fn run<A>(app: A, mouse: bool, run_app: impl FnOnce(A) -> io::Result<()>) -> io::Result<()> {
    run_with_lease(app, || enter(mouse), run_app, Guard::finish)
}

/// This is also the fault-injection seam for the lifecycle regressions. The
/// public entry point above and tests execute exactly the same ordering.
fn run_with_lease<A, G>(
    app: A,
    acquire: impl FnOnce() -> io::Result<G>,
    run_app: impl FnOnce(A) -> io::Result<()>,
    finish: impl FnOnce(G) -> io::Result<()>,
) -> io::Result<()> {
    let guard = match acquire() {
        Ok(guard) => guard,
        Err(error) => {
            drop(app);
            return Err(error);
        }
    };
    let result = run_app(app);
    // Deliberately not `result?`: even a failed App::run must restore the
    // original mode, and a cleanup failure must not erase the primary cause.
    let restored = finish(guard);
    combine_results(result, restored)
}

/// Preserve the primary error kind and both diagnostics when cleanup fails
/// too. Used after `App::run` and after a fatal model-side terminal failure.
pub fn combine_results(primary: io::Result<()>, cleanup: io::Result<()>) -> io::Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(io::Error::new(
            primary.kind(),
            format!("{primary}; additional terminal error: {cleanup}"),
        )),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Acquire native input ownership before FTUI performs its first output write.
#[cfg_attr(
    not(windows),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
pub fn enter(mouse: bool) -> io::Result<Guard> {
    #[cfg(windows)]
    {
        native::begin(mouse)?;
        let guard = Guard {
            active: true,
            _thread_bound: PhantomData,
        };
        // Failure unwinds the guard and restores the original mode.
        resume()?;
        Ok(guard)
    }
    #[cfg(not(windows))]
    {
        let _ = mouse;
        Ok(Guard {
            active: false,
            _thread_bound: PhantomData,
        })
    }
}

/// Restore the shell mode while a synchronous external editor owns the console.
#[cfg_attr(
    not(windows),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
pub fn suspend() -> io::Result<()> {
    #[cfg(windows)]
    {
        native::suspend()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Reapply native input flags after raw mode is re-enabled, before rendering.
#[cfg_attr(
    not(windows),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
pub fn resume() -> io::Result<()> {
    #[cfg(windows)]
    {
        native::resume()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

#[cfg_attr(
    not(windows),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
fn restore() -> io::Result<()> {
    #[cfg(windows)]
    {
        native::restore()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::{EXTENDED_FLAGS, ModeLease, io};
    use std::cell::RefCell;
    use std::fs::OpenOptions;
    use winapi_util::{Handle, console};

    struct Session {
        input: Handle,
        lease: ModeLease,
    }

    thread_local! {
        static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    }

    pub(super) fn begin(mouse: bool) -> io::Result<()> {
        SESSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "FTUI already owns this thread's console input mode",
                ));
            }
            // CONIN$ is independent of redirected stdin. Both access rights
            // are intentional; do not regress issue #239's access-denied fix.
            let input =
                Handle::from_file(OpenOptions::new().read(true).write(true).open("CONIN$")?);
            let original = console::mode(&input)?;
            tracing::debug!(target: "pi::ftui::console", original, mouse,
                "captured shell console input mode");
            *slot = Some(Session {
                input,
                lease: ModeLease { original, mouse },
            });
            Ok(())
        })
    }

    fn set_verified(session: &Session, mode: u32, phase: &'static str) -> io::Result<()> {
        console::set_mode(&session.input, mode)?;
        let actual = console::mode(&session.input)?;
        tracing::debug!(target: "pi::ftui::console", phase,
            original = session.lease.original, requested = mode, actual,
            "console input mode transition");
        if actual != mode {
            return Err(io::Error::other(format!(
                "FTUI console {phase}: requested mode {mode:#06x}, read back {actual:#06x}"
            )));
        }
        Ok(())
    }

    fn restore_session(session: &Session) -> io::Result<()> {
        let [with_extended, original] = session.lease.restore_steps();
        set_verified(session, with_extended, "restore-extended")?;
        if original & EXTENDED_FLAGS == 0 {
            set_verified(session, original, "restore")?;
        }
        Ok(())
    }

    pub(super) fn suspend() -> io::Result<()> {
        SESSION.with(|slot| {
            if let Some(session) = slot.borrow().as_ref() {
                restore_session(session)?;
            }
            Ok(())
        })
    }

    pub(super) fn resume() -> io::Result<()> {
        SESSION.with(|slot| {
            if let Some(session) = slot.borrow().as_ref() {
                let current = console::mode(&session.input)?;
                set_verified(session, session.lease.capture(current), "capture")?;
            }
            Ok(())
        })
    }

    pub(super) fn restore() -> io::Result<()> {
        SESSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(session) = slot.as_ref() {
                restore_session(session)?;
            }
            // Retain the original baseline on failure so Guard::drop retries.
            *slot = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Model the documented SetConsoleMode requirement rather than treating
    // mode writes as ordinary assignment. This catches a missing EXTENDED bit.
    fn conhost_set_mode(current: u32, requested: u32) -> u32 {
        const INSERT_MODE: u32 = 0x0020;
        let extended_modes = QUICK_EDIT | INSERT_MODE;
        if requested & EXTENDED_FLAGS == 0 {
            (requested & !extended_modes) | (current & extended_modes)
        } else {
            requested
        }
    }

    #[test]
    fn raw_mode_alone_leaves_the_quick_edit_failure_precondition() {
        const COOKED: u32 = 0x0007;
        let shell = COOKED | QUICK_EDIT | EXTENDED_FLAGS;
        let raw = shell & !COOKED;
        assert_ne!(raw & QUICK_EDIT, 0);
        // Merely clearing QuickEdit in a mode without EXTENDED is not a fix.
        assert_ne!(conhost_set_mode(raw, 0) & QUICK_EDIT, 0);
        let lease = ModeLease {
            original: shell,
            mouse: true,
        };
        let captured = conhost_set_mode(raw, lease.capture(raw));
        assert_eq!(captured & QUICK_EDIT, 0);
        assert_eq!(captured & COOKED, 0);
        assert_eq!(
            captured & (WINDOW_INPUT | MOUSE_INPUT),
            WINDOW_INPUT | MOUSE_INPUT
        );
    }

    #[test]
    fn capture_preserves_every_unowned_bit() {
        let owned = WINDOW_INPUT | MOUSE_INPUT | QUICK_EDIT | EXTENDED_FLAGS;
        for low in 0..=0x03ff {
            let current = low | 0x8000_0000;
            let lease = ModeLease {
                original: current,
                mouse: true,
            };
            let captured = conhost_set_mode(current, lease.capture(current));
            assert_eq!(captured & !owned, current & !owned);
            assert_eq!(captured & QUICK_EDIT, 0);
            assert_eq!(lease.capture(captured), captured);
        }
    }

    #[test]
    fn restoration_handles_baselines_without_extended_flags() {
        for original in 0..=0x03ff {
            let lease = ModeLease {
                original,
                mouse: true,
            };
            let mut mode = lease.capture(original ^ QUICK_EDIT);
            for requested in lease.restore_steps() {
                mode = conhost_set_mode(mode, requested);
            }
            assert_eq!(mode, original);
        }
    }

    #[test]
    fn child_changes_never_replace_the_original_baseline() {
        let original = 0x0007 | QUICK_EDIT;
        let lease = ModeLease {
            original,
            mouse: true,
        };
        let mut current = lease.capture(original & !0x0007);
        for child in [0x03ff, 0, QUICK_EDIT, EXTENDED_FLAGS | QUICK_EDIT] {
            for requested in lease.restore_steps() {
                current = conhost_set_mode(current, requested);
            }
            assert_eq!(current, original);
            current = lease.capture(child & !0x0007);
            assert_eq!(current & QUICK_EDIT, 0);
            assert_eq!(current & 0x0007, 0);
        }
        for requested in lease.restore_steps() {
            current = conhost_set_mode(current, requested);
        }
        assert_eq!(current, original);
    }

    #[test]
    fn mouse_opt_out_preserves_native_selection_policy() {
        for original in 0..=0x03ff {
            let lease = ModeLease {
                original,
                mouse: false,
            };
            assert_eq!(
                lease.capture(original),
                (original | WINDOW_INPUT) & !MOUSE_INPUT
            );
        }
    }

    #[test]
    fn failed_acquisition_disconnects_the_driver_without_running_the_app() {
        let (submit_tx, submit_rx) = std::sync::mpsc::channel::<()>();
        let result = run_with_lease(
            submit_tx,
            || Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied)),
            |_| panic!("an app with no console lease must not run"),
            |()| panic!("failed acquisition did not produce a lease"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            submit_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected),
            "the caller must be able to join the driver immediately after return"
        );
    }

    #[test]
    fn cleanup_runs_after_the_app_releases_its_channels_even_on_error() {
        for fail_run in [false, true] {
            let (submit_tx, submit_rx) = std::sync::mpsc::channel::<()>();
            let restored = std::cell::Cell::new(false);
            let result = run_with_lease(
                submit_tx,
                || Ok(()),
                |app| {
                    drop(app);
                    if fail_run {
                        Err(io::Error::from(io::ErrorKind::BrokenPipe))
                    } else {
                        Ok(())
                    }
                },
                |()| {
                    assert_eq!(
                        submit_rx.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Disconnected)
                    );
                    restored.set(true);
                    Ok(())
                },
            );
            assert!(restored.get(), "cleanup cannot be skipped by an app error");
            assert_eq!(result.is_err(), fail_run);
        }
    }

    #[test]
    fn app_and_restore_failures_keep_both_diagnostics() {
        let error = run_with_lease(
            (),
            || Ok(()),
            |()| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "frame write failed",
                ))
            },
            |()| Err(io::Error::other("shell mode restore failed")),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("frame write failed"));
        assert!(error.to_string().contains("shell mode restore failed"));
    }

    #[test]
    fn a_restore_failure_cannot_be_reported_as_a_successful_exit() {
        let error = run_with_lease(
            (),
            || Ok(()),
            |()| Ok(()),
            |()| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_outer_lease_is_dropped_if_the_app_unwinds() {
        struct Lease<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Lease<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let restored = std::cell::Cell::new(false);
        let (submit_tx, submit_rx) = std::sync::mpsc::channel::<()>();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_lease(
                submit_tx,
                || Ok(Lease(&restored)),
                |_app| panic!("simulated runtime unwind"),
                |_| panic!("normal finish must not run while unwinding"),
            )
        }));
        assert!(outcome.is_err());
        assert!(restored.get());
        assert_eq!(
            submit_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_run_wrapper_is_headless_off_windows() {
        let ran = std::cell::Cell::new(false);
        run((), true, |()| {
            ran.set(true);
            Ok(())
        })
        .expect("non-Windows wrapper must not open a terminal");
        assert!(ran.get());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_lifecycle_does_not_touch_a_terminal() {
        let guard = enter(true).expect("no-op acquire");
        suspend().expect("no-op suspend");
        resume().expect("no-op resume");
        guard.finish().expect("no-op restore");
    }
}
