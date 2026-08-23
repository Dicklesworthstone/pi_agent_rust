//! Crash capture: redacted crash bundles from panics and fatal signals
//! (bd-cv653.7.12).
//!
//! Bundles land under `<agent-dir>/crashes/<stamp>/` as `bundle.json`
//! (`pi.crash.v1`) plus a human-readable `report.txt`. Every free-text field
//! is redacted through [`crate::secrets::scan`] before it touches disk, so
//! credential-shaped strings never persist (privacy acceptance #4).
//!
//! Nothing is transmitted automatically; `send` surfaces a payload preview
//! and leaves transport to the operator.
//!
//! Signal coverage note: with `forbid(unsafe_code)` we use `signal-hook`'s
//! safe registration. A `SIGSEGV` may still terminate the process before the
//! watcher thread finishes writing; those bundles are best-effort and record
//! the signal name plus the redacted operation ring (no backtrace).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// Schema tag for crash bundles.
pub const CRASH_SCHEMA: &str = "pi.crash.v1";
/// Ring capacity for recent-operation context.
const RING_CAPACITY: usize = 64;
/// Directory name inside the agent dir.
pub const CRASHES_DIR_NAME: &str = "crashes";

static RING: std::sync::Mutex<Option<VecDeque<String>>> = std::sync::Mutex::new(None);
static INSTALLED: AtomicBool = AtomicBool::new(false);
thread_local! {
    /// Set while running code whose panics are recovered internally
    /// (`catch_unwind` sites such as background compaction). The panic hook
    /// skips bundle capture for these — a caught panic is not a crash.
    static PANIC_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the current thread's panics as expected-and-recovered.
///
/// bd-ajg8l #3: background compaction and similar `catch_unwind` sites
/// must not produce "previous run crashed" bundles. Suppression holds for
/// the guard's lifetime; the crash hook returns early (no bundle, no
/// chained hook) because the recovery is intentional.
#[must_use]
pub struct SuppressPanicHook;

impl SuppressPanicHook {
    pub fn new() -> Self {
        PANIC_SUPPRESSED.with(|flag| flag.set(true));
        Self
    }
}

impl Default for SuppressPanicHook {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SuppressPanicHook {
    fn drop(&mut self) {
        PANIC_SUPPRESSED.with(|flag| flag.set(false));
    }
}
/// Record an operation into the redacted-at-capture recent-operations ring
/// that crash bundles include as context. Cheap; capped at
/// [`RING_CAPACITY`] entries.
pub fn record_operation(operation: impl Into<String>) {
    let entry = redact_text(&operation.into());
    if let Ok(mut guard) = RING.lock() {
        let ring = guard.get_or_insert_with(VecDeque::new);
        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry);
    }
}

fn ring_tail() -> Vec<String> {
    RING.lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|ring| ring.iter().cloned().collect()))
        .unwrap_or_default()
}

/// Redact credential-shaped spans through the shared detector so tool and
/// crash-path redaction cannot drift.
#[must_use]
pub fn redact_text(text: &str) -> String {
    let mut detections = crate::secrets::scan(text, &[]);
    if detections.is_empty() {
        return text.to_string();
    }
    detections.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut index = 0usize;
    while index < detections.len() {
        let first = &detections[index];
        if first.start < cursor {
            index += 1;
            continue;
        }
        // Fold every detection overlapping this run into one marker so
        // nested/overlapping rules cannot produce out-of-range spans.
        let mut end = first.end;
        let mut lookahead = index + 1;
        while lookahead < detections.len() && detections[lookahead].start < end {
            end = end.max(detections[lookahead].end);
            lookahead += 1;
        }
        let safe_start = first.start.min(text.len());
        let safe_end = end.min(text.len()).max(safe_start);
        out.push_str(&text[cursor..safe_start]);
        let _ = write!(out, "[REDACTED:{}]", first.rule);
        cursor = safe_end;
        index = lookahead;
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

/// Crash bundle written to disk (`pi.crash.v1`).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct CrashBundle {
    pub schema: String,
    /// `panic` or `signal:<name>`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panic_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backtrace: Option<String>,
    pub build_git_sha: String,
    pub build_timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub recent_operations: Vec<String>,
    pub created_at: String,
}

impl CrashBundle {
    /// Human-readable multi-line rendering for `show`.
    #[must_use]
    pub fn render_report(&self) -> String {
        let mut out = format!("pi crash bundle ({})\n", self.kind);
        if let Some(message) = &self.panic_message {
            let _ = writeln!(out, "panic: {message}");
        }
        if let Some(backtrace) = &self.backtrace {
            out.push_str("\nbacktrace:\n");
            out.push_str(backtrace);
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "build: {} @ {}\nsession: {}\ncreated: {}",
            self.build_git_sha,
            self.build_timestamp,
            self.session_path.as_deref().unwrap_or("(none)"),
            self.created_at
        );
        if !self.recent_operations.is_empty() {
            out.push_str("\nrecent operations (redacted):\n");
            for op in &self.recent_operations {
                let _ = writeln!(out, "  - {op}");
            }
        }
        out
    }
}

fn utc_stamp() -> String {
    // chrono is already a workspace dependency (session timestamps).
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn stamp_for_dir() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ%3f").to_string()
}

fn build_metadata() -> (String, String) {
    (
        option_env!("VERGEN_GIT_SHA")
            .unwrap_or("unknown")
            .to_string(),
        option_env!("VERGEN_BUILD_TIMESTAMP")
            .unwrap_or("unknown")
            .to_string(),
    )
}

fn crashes_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(CRASHES_DIR_NAME)
}

/// Write a bundle directory and return its path.
fn write_bundle(agent_dir: &Path, mut bundle: CrashBundle) -> Result<PathBuf, String> {
    bundle.recent_operations = bundle
        .recent_operations
        .into_iter()
        .map(|op| redact_text(&op))
        .collect();
    bundle.panic_message = bundle.panic_message.as_deref().map(redact_text);
    bundle.backtrace = bundle.backtrace.as_deref().map(redact_text);

    let dir = crashes_dir(agent_dir).join(stamp_for_dir());
    std::fs::create_dir_all(&dir).map_err(|e| format!("create crash dir: {e}"))?;
    // Bundles carry backtraces and the operations ring; keep them
    // owner-only like session JSONLs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let json =
        serde_json::to_string_pretty(&bundle).map_err(|e| format!("serialize bundle: {e}"))?;
    std::fs::write(dir.join("bundle.json"), json).map_err(|e| format!("write bundle.json: {e}"))?;
    let mut report = std::fs::File::create(dir.join("report.txt"))
        .map_err(|e| format!("create report.txt: {e}"))?;
    let _ = report.write_all(bundle.render_report().as_bytes());
    Ok(dir)
}

/// Install the panic hook (chaining any previously installed hook) and start
/// the fatal-signal watcher thread. Idempotent.
pub fn install(agent_dir: &Path, session_path: Option<&Path>) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let agent_dir = agent_dir.to_path_buf();
    let session_path_redacted = session_path.map(|p| redact_text(&p.display().to_string()));
    let hook_dir = agent_dir.clone();
    let hook_session = session_path_redacted.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Recovered-internal panics (catch_unwind sites) are not crashes.
        if PANIC_SUPPRESSED.with(std::cell::Cell::get) {
            return;
        }
        let (sha, ts) = build_metadata();
        let message = redact_text(&payload_of(info));
        let bundle = CrashBundle {
            schema: CRASH_SCHEMA.to_string(),
            kind: "panic".into(),
            panic_message: Some(message),
            backtrace: Some(std::backtrace::Backtrace::force_capture().to_string()),
            build_git_sha: sha,
            build_timestamp: ts,
            session_path: hook_session.clone(),
            created_at: utc_stamp(),
            recent_operations: ring_tail(),
        };
        let _ = write_bundle(&hook_dir, bundle);
        previous(info);
    }));
    spawn_signal_watcher(agent_dir, session_path_redacted);
}

fn payload_of(info: &std::panic::PanicHookInfo<'_>) -> String {
    info.payload().downcast_ref::<&str>().map_or_else(
        || {
            info.payload()
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "unknown panic payload".into())
        },
        |payload| (*payload).to_string(),
    )
}

/// Best-effort fatal-signal watcher: writes a minimal bundle naming the
/// signal with redacted ring context. See module docs for the coverage
/// caveat under `forbid(unsafe_code)`.
fn spawn_signal_watcher(agent_dir: PathBuf, session_path: Option<String>) {
    // SIGSEGV/SIGILL/SIGFPE are forbidden by signal-hook's safe registry
    // (registration panics, not errors) — the module docs already scope
    // segfault coverage as best-effort-absent under `forbid(unsafe_code)`.
    let watched = [
        signal_hook::consts::signal::SIGABRT,
        signal_hook::consts::signal::SIGBUS,
    ];
    let Ok(mut signals) = signal_hook::iterator::Signals::new(watched) else {
        tracing::warn!(event = "pi.crash.watch", "signal watcher unavailable");
        return;
    };
    std::thread::Builder::new()
        .name("pi-crash-watch".into())
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                let (sha, ts) = build_metadata();
                let bundle = CrashBundle {
                    schema: CRASH_SCHEMA.to_string(),
                    kind: format!("signal:{signal}"),
                    panic_message: None,
                    backtrace: None,
                    build_git_sha: sha,
                    build_timestamp: ts,
                    session_path: session_path.clone(),
                    recent_operations: ring_tail(),
                    created_at: utc_stamp(),
                };
                let _ = write_bundle(&agent_dir, bundle);
                // Fatal signals must stay fatal: signal-hook's iterator
                // replaces the default disposition, so without this the
                // process would survive SIGABRT/SIGBUS (and a hardware
                // SIGBUS would re-deliver forever). Record, then die the
                // way the default handler would have.
                let _ = signal_hook::low_level::emulate_default_handler(signal);
            }
        })
        .ok();
}

/// One-line summary of an existing bundle directory.
#[derive(Debug, Clone)]
pub struct BundleSummary {
    pub dir: PathBuf,
    pub kind: String,
    pub created_at: String,
    pub noticed: bool,
}

/// List bundle directories under `agent_dir`, oldest first.
pub fn list_bundles(agent_dir: &Path) -> Vec<BundleSummary> {
    let root = crashes_dir(agent_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|dir| {
            let raw = std::fs::read_to_string(dir.join("bundle.json")).ok()?;
            let bundle: CrashBundle = serde_json::from_str(&raw).ok()?;
            let noticed = dir.join("noticed").exists();
            Some(BundleSummary {
                dir,
                kind: bundle.kind,
                created_at: bundle.created_at,
                noticed,
            })
        })
        .collect()
}

/// Print one-line notices for bundles not yet acknowledged and mark them
/// noticed. Returns how many new bundles were surfaced.
pub fn emit_startup_notice(agent_dir: &Path) -> usize {
    let mut surfaced = 0;
    for summary in list_bundles(agent_dir) {
        if summary.noticed {
            continue;
        }
        eprintln!(
            "note: previous run crashed ({}) — bundle: {} (use /crash show)",
            summary.kind,
            summary.dir.display()
        );
        let _ = std::fs::write(summary.dir.join("noticed"), "");
        surfaced += 1;
    }
    surfaced
}

/// Render the newest bundle's human report, marking it noticed.
pub fn show_latest(agent_dir: &Path) -> Option<String> {
    let latest = list_bundles(agent_dir).pop()?;
    let report = std::fs::read_to_string(latest.dir.join("report.txt")).ok()?;
    let _ = std::fs::write(latest.dir.join("noticed"), "");
    Some(report)
}

/// Delete every recorded bundle directory. Returns how many were removed.
pub fn delete_all(agent_dir: &Path) -> usize {
    let bundles = list_bundles(agent_dir);
    let mut removed = 0;
    for summary in bundles {
        if std::fs::remove_dir_all(&summary.dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Payload preview for explicit `send` — nothing is transmitted here; the
/// caller shows this to the operator and provides transport themselves.
#[must_use]
pub fn send_preview(agent_dir: &Path) -> Option<String> {
    let latest = list_bundles(agent_dir).pop()?;
    let raw = std::fs::read_to_string(latest.dir.join("bundle.json")).ok()?;
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_dir(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("pi-crash-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        base
    }

    #[test]
    fn redaction_masks_credential_shapes() {
        let text = "reading ANTHROPIC_API_KEY=sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaa done";
        let redacted = redact_text(text);
        assert!(!redacted.contains("sk-ant-api03"), "{redacted}");
        assert!(redacted.contains("[REDACTED:"), "{redacted}");
    }

    #[test]
    fn panic_bundle_round_trips_with_schema_and_redaction() {
        let dir = agent_dir("panic-bundle");
        let bundle = CrashBundle {
            schema: CRASH_SCHEMA.to_string(),
            kind: "panic".into(),
            panic_message: Some(
                "boom at ANTHROPIC_API_KEY=sk-ant-api03-bbbbbbbbbbbbbbbbbbbb".into(),
            ),
            backtrace: None,
            build_git_sha: "testsha".into(),
            build_timestamp: "t".into(),
            session_path: None,
            recent_operations: vec![
                "read ~/.env with SECRET_TOKEN=ghp_cccccccccccccccccccc".into(),
            ],
            created_at: utc_stamp(),
        };
        let bundle_dir = write_bundle(&dir, bundle).expect("write");

        let raw = std::fs::read_to_string(bundle_dir.join("bundle.json")).unwrap();
        assert!(raw.contains(CRASH_SCHEMA));
        assert!(!raw.contains("sk-ant-api03"), "canary leaked: {raw}");
        assert!(!raw.contains("ghp_ccccc"), "ring canary leaked: {raw}");

        let summaries = list_bundles(&dir);
        assert_eq!(summaries.len(), 1);
        assert!(!summaries[0].noticed);

        let shown = show_latest(&dir).expect("show");
        assert!(shown.contains("panic:"), "{shown}");
        assert!(list_bundles(&dir)[0].noticed, "show marks noticed");
    }

    #[test]
    fn notice_once_then_delete() {
        let dir = agent_dir("notice-once");
        let bundle = CrashBundle {
            schema: CRASH_SCHEMA.to_string(),
            kind: "signal:4".into(),
            panic_message: None,
            backtrace: None,
            build_git_sha: "s".into(),
            build_timestamp: "t".into(),
            session_path: None,
            recent_operations: vec![],
            created_at: utc_stamp(),
        };
        let _ = write_bundle(&dir, bundle);

        assert_eq!(emit_startup_notice(&dir), 1, "first notice surfaces");
        assert_eq!(emit_startup_notice(&dir), 0, "second launch stays quiet");
        assert_eq!(delete_all(&dir), 1);
        assert!(list_bundles(&dir).is_empty());
    }

    #[test]
    fn send_preview_never_transmits_and_masks_secrets() {
        let dir = agent_dir("send-preview");
        let bundle = CrashBundle {
            schema: CRASH_SCHEMA.to_string(),
            kind: "panic".into(),
            panic_message: Some("token GITHUB_TOKEN=ghp_dddddddddddddddddddd in scope".into()),
            backtrace: None,
            build_git_sha: "s".into(),
            build_timestamp: "t".into(),
            session_path: None,
            recent_operations: vec![],
            created_at: utc_stamp(),
        };
        let _ = write_bundle(&dir, bundle);
        let preview = send_preview(&dir).expect("preview");
        assert!(preview.contains(CRASH_SCHEMA));
        assert!(!preview.contains("ghp_dddddd"), "{preview}");
    }

    #[test]
    fn suppressed_panics_do_not_write_bundles() {
        // bd-ajg8l #3: recovered-internal panics (catch_unwind sites such as
        // background compaction) must not produce "previous run crashed"
        // bundles.
        let dir = agent_dir("suppress");
        install(&dir, None);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = SuppressPanicHook::new();
            panic!("recovered internal panic");
        }));
        assert!(result.is_err(), "panic must still unwind to the catcher");
        assert!(
            list_bundles(&dir).is_empty(),
            "suppressed panic must not write a bundle"
        );

        // Unsuppressed panics still capture.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("unrecovered probe");
        }));
        assert_eq!(list_bundles(&dir).len(), 1, "unsuppressed panic captures");
        let _ = delete_all(&dir);
    }
    #[test]
    fn ring_is_capped() {
        for index in 0..(RING_CAPACITY * 2) {
            record_operation(format!("op-{index}"));
        }
        let tail = ring_tail();
        assert_eq!(tail.len(), RING_CAPACITY);
        assert!(tail.last().unwrap().starts_with("op-1"), "newest kept");
    }
}
