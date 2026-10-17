//! Minimal real-FTUI `ConHost` probe with an independent, file-only heartbeat.
//! See docs/investigations/issue-239-conhost.md before interpreting its output.
#![forbid(unsafe_code)]

#[cfg(feature = "ftui")]
#[path = "../src/interactive_ftui/console_input.rs"]
mod console_input;

#[cfg(feature = "ftui")]
mod enabled {
    use super::console_input;
    use ftui::core::event::KeyEventKind;
    use ftui::prelude::*;
    use ftui::widgets::paragraph::Paragraph;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    struct Progress {
        start: Instant,
        phase: AtomicU8,
        completed_ms: AtomicU64,
        ticks: AtomicU64,
        keys: AtomicU64,
        resizes: AtomicU64,
        views: AtomicU64,
        observer_failed: AtomicBool,
        error: Mutex<Option<String>>,
    }

    impl Progress {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                phase: AtomicU8::new(0),
                completed_ms: AtomicU64::new(0),
                ticks: AtomicU64::new(0),
                keys: AtomicU64::new(0),
                resizes: AtomicU64::new(0),
                views: AtomicU64::new(0),
                observer_failed: AtomicBool::new(false),
                error: Mutex::new(None),
            }
        }

        fn elapsed_ms(&self) -> u64 {
            u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
        }

        fn completed(&self) {
            self.completed_ms
                .store(self.elapsed_ms(), Ordering::Relaxed);
            self.phase.store(3, Ordering::Relaxed);
        }

        fn record(&self, mode: io::Result<Option<u32>>, native_fix: bool) -> serde_json::Value {
            let elapsed = self.elapsed_ms();
            let (mode, mode_error) = match mode {
                Ok(mode) => (mode, None),
                Err(error) => (None, Some(error.to_string())),
            };
            let phase = match self.phase.load(Ordering::Relaxed) {
                0 => "starting",
                1 => "update",
                2 => "view",
                4 => "finished",
                _ => "outside-model",
            };
            serde_json::json!({
                "schema": "pi.ftui.conhost_probe.v1", "elapsed_ms": elapsed,
                "native_input_fix": native_fix, "phase": phase,
                "callback_age_ms": elapsed.saturating_sub(self.completed_ms.load(Ordering::Relaxed)),
                "ticks": self.ticks.load(Ordering::Relaxed),
                "key_events": self.keys.load(Ordering::Relaxed),
                "resize_events": self.resizes.load(Ordering::Relaxed),
                "views": self.views.load(Ordering::Relaxed),
                "input_mode": mode, "input_mode_error": mode_error,
                "quick_edit": mode.map(|mode| mode & 0x0040 != 0),
                "processed_input": mode.map(|mode| mode & 0x0001 != 0),
                "line_input": mode.map(|mode| mode & 0x0002 != 0),
                "echo_input": mode.map(|mode| mode & 0x0004 != 0),
                "mouse_input": mode.map(|mode| mode & 0x0010 != 0),
                "window_input": mode.map(|mode| mode & 0x0008 != 0)
            })
        }
    }

    #[cfg_attr(
        not(windows),
        allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
    )]
    fn input_mode() -> io::Result<Option<u32>> {
        #[cfg(windows)]
        {
            let handle = winapi_util::Handle::from_file(
                OpenOptions::new().read(true).write(true).open("CONIN$")?,
            );
            winapi_util::console::mode(&handle).map(Some)
        }
        #[cfg(not(windows))]
        {
            Ok(None)
        }
    }

    struct Observer {
        stop: mpsc::Sender<()>,
        worker: Option<JoinHandle<io::Result<()>>>,
    }

    impl Observer {
        fn start(mut log: File, state: Arc<Progress>, native_fix: bool) -> io::Result<Self> {
            let (stop, stopped) = mpsc::channel();
            let worker = thread::Builder::new()
                .name("conhost-observer".into())
                .spawn(move || {
                    let result = (|| {
                        loop {
                            // Only GetConsoleMode and file writes: never poll/read
                            // terminal events, and never contend for stdout/stderr.
                            writeln!(log, "{}", state.record(input_mode(), native_fix))?;
                            log.flush()?;
                            if !matches!(
                                stopped.recv_timeout(Duration::from_millis(500)),
                                Err(mpsc::RecvTimeoutError::Timeout)
                            ) {
                                writeln!(log, "{}", state.record(input_mode(), native_fix))?;
                                return log.flush();
                            }
                        }
                    })();
                    if result.is_err() {
                        state.observer_failed.store(true, Ordering::Release);
                    }
                    result
                })?;
            Ok(Self {
                stop,
                worker: Some(worker),
            })
        }

        fn finish(mut self) -> io::Result<()> {
            let _ = self.stop.send(());
            if let Some(worker) = self.worker.take() {
                return worker
                    .join()
                    .map_err(|_| io::Error::other("console observer panicked"))?;
            }
            Ok(())
        }
    }

    impl Drop for Observer {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    struct Probe {
        state: Arc<Progress>,
    }

    impl Model for Probe {
        type Message = Event;

        fn init(&mut self) -> Cmd<Event> {
            Cmd::tick(Duration::from_millis(250))
        }

        fn update(&mut self, msg: Event) -> Cmd<Event> {
            self.state.phase.store(1, Ordering::Relaxed);
            let command = match msg {
                Event::Tick => {
                    self.state.ticks.fetch_add(1, Ordering::Relaxed);
                    if self.state.observer_failed.load(Ordering::Acquire) {
                        Cmd::quit()
                    } else {
                        Cmd::tick(Duration::from_millis(250))
                    }
                }
                Event::Key(key) => {
                    self.state.keys.fetch_add(1, Ordering::Relaxed);
                    // Keep releases visible in the event count, but do not
                    // cycle terminal modes twice for one Windows keystroke.
                    if key.kind == KeyEventKind::Release {
                        Cmd::none()
                    } else if key.is_char('q') {
                        Cmd::quit()
                    } else if key.is_char('s') {
                        // Exercise the native-input portion of an editor
                        // handoff synchronously; no child process is launched.
                        let result = crossterm::terminal::disable_raw_mode()
                            .and_then(|()| console_input::suspend())
                            .and_then(|()| crossterm::terminal::enable_raw_mode())
                            .and_then(|()| console_input::resume());
                        if let Err(error) = result {
                            *self
                                .state
                                .error
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(error.to_string());
                            Cmd::quit()
                        } else {
                            Cmd::none()
                        }
                    } else {
                        Cmd::none()
                    }
                }
                Event::Resize { .. } => {
                    self.state.resizes.fetch_add(1, Ordering::Relaxed);
                    Cmd::none()
                }
                _ => Cmd::none(),
            };
            self.state.completed();
            command
        }

        fn view(&self, frame: &mut Frame) {
            self.state.phase.store(2, Ordering::Relaxed);
            let text = format!(
                "FTUI ConHost probe: ticks={} key-events={} resizes={}\n\
                 Type, resize and click/drag to reproduce. q quits; s cycles input modes.\n\
                 Only event counts and console mode bits are written to the trace.",
                self.state.ticks.load(Ordering::Relaxed),
                self.state.keys.load(Ordering::Relaxed),
                self.state.resizes.load(Ordering::Relaxed)
            );
            Paragraph::new(text).render(Rect::new(0, 0, frame.width(), frame.height()), frame);
            self.state.views.fetch_add(1, Ordering::Relaxed);
            self.state.completed();
        }
    }

    pub fn run() -> io::Result<()> {
        let mut path = None;
        let mut native_fix = false;
        let mut fullscreen = false;
        for arg in std::env::args_os().skip(1) {
            if arg == "--native-input" {
                native_fix = true;
            } else if arg == "--fullscreen" {
                fullscreen = true;
            } else if path.is_none() && !arg.to_string_lossy().starts_with('-') {
                path = Some(PathBuf::from(arg));
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: ftui_conhost_probe TRACE.jsonl [--native-input] [--fullscreen]",
                ));
            }
        }
        let path = path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a new trace file path is required",
            )
        })?;
        #[cfg(not(windows))]
        if native_fix {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "--native-input requires a real Windows console",
            ));
        }
        // Never truncate an existing trace. No provider, credentials, Pi
        // session, typed characters, or terminal input bytes are collected.
        let mut log = OpenOptions::new().write(true).create_new(true).open(path)?;
        let state = Arc::new(Progress::new());
        writeln!(log, "{}", state.record(input_mode(), native_fix))?;
        log.flush()?;
        let observer = Observer::start(log, Arc::clone(&state), native_fix)?;
        let model = Probe {
            state: Arc::clone(&state),
        };
        let app = if fullscreen {
            App::fullscreen(model)
        } else {
            App::inline_auto(model, 3, 8)
        };
        // Exercise the same lease runner as the pending production integration:
        // drop an unrun app on acquisition failure, and restore after App::run
        // even when it fails. Baseline runs intentionally leave native modes alone.
        let result = if native_fix {
            console_input::run(app, true, |app| app.with_mouse().run())
        } else {
            app.with_mouse().run()
        };
        state.phase.store(4, Ordering::Relaxed);
        let observer_result = observer.finish();
        result?;
        observer_result?;
        let failure = state
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(error) = failure {
            return Err(io::Error::other(error));
        }
        Ok(())
    }
}

#[cfg(feature = "ftui")]
fn main() -> std::io::Result<()> {
    enabled::run()
}

#[cfg(not(feature = "ftui"))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "ftui_conhost_probe requires the ftui feature",
    ))
}
