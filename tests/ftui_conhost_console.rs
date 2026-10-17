//! Console-mode policy regressions for issue #239.
//!
//! The portable tests live next to the implementation and model Windows'
//! `EXTENDED_FLAGS` semantics. The native test requires exclusive ownership of
//! a real Windows console; it must not run beside another terminal consumer.
#![cfg(feature = "ftui")]

#[path = "../src/interactive_ftui/console_input.rs"]
mod console_input;

#[cfg(windows)]
#[test]
#[ignore = "requires an exclusive real Windows console, not a redirected test runner"]
fn native_console_input_is_restored_across_editor_handoffs() -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use winapi_util::{Handle, console};

    struct RestoreShell {
        input: Handle,
        mode: u32,
    }
    impl Drop for RestoreShell {
        fn drop(&mut self) {
            let _ = console::set_mode(&self.input, self.mode | 0x0080);
            let _ = console::set_mode(&self.input, self.mode);
        }
    }

    let input = Handle::from_file(OpenOptions::new().read(true).write(true).open("CONIN$")?);
    let mode = console::mode(&input)?;
    let shell = RestoreShell { input, mode };
    // Seed the failure precondition without selecting text or reading keys.
    let baseline = (mode | 0x00c7) & !0x0018;
    console::set_mode(&shell.input, baseline)?;
    let guard = console_input::enter(true)?;
    assert!(
        console_input::enter(true).is_err(),
        "nested lease must be rejected"
    );
    crossterm::terminal::enable_raw_mode()?;
    console_input::resume()?;
    let active = console::mode(&shell.input)?;
    assert_eq!(
        active & 0x0047,
        0,
        "no QuickEdit, cooked input or OS Ctrl+C"
    );
    assert_eq!(active & 0x0098, 0x0098, "native mouse/resize input enabled");

    for _ in 0..3 {
        crossterm::terminal::disable_raw_mode()?;
        console_input::suspend()?;
        assert_eq!(console::mode(&shell.input)?, baseline);
        // Simulate an editor changing the shared console mode. This must not
        // become the next saved shell baseline.
        console::set_mode(&shell.input, baseline | 0x0010)?;
        crossterm::terminal::enable_raw_mode()?;
        console_input::resume()?;
        assert_eq!(console::mode(&shell.input)? & 0x0047, 0);
    }
    crossterm::terminal::disable_raw_mode()?;
    guard.finish()?;
    assert_eq!(console::mode(&shell.input)?, baseline);
    Ok(())
}
