//! Fault-injection e2e persistence scripts with detailed trace logs (bd-3ar8v.2.10).
//!
//! These tests inject crashes, interruptions, and corruption at persistence
//! boundaries and validate that the session recovery path produces correct
//! state with structured trace diagnostics.
//!
//! Unlike the unit-level crash-consistency tests in `tests/crash_consistency.rs`
//! which test individual recovery primitives, these tests exercise full
//! end-to-end persistence lifecycles:
//!
//! - Multi-phase append → crash → recover → continue → verify cycles
//! - Checkpoint healing of accumulated corruption
//! - Stale temp file cleanup after interrupted atomic rewrites
//! - Autosave queue state machine under fault injection
//! - V2 store segment/index consistency after fault injection
//! - Cross-durability-mode fault behavior
//! - Trace log correlation for debugging persistence failures

use asupersync::runtime::RuntimeBuilder;
use pi::model::UserContent;
use pi::session::{AutosaveDurabilityMode, AutosaveFlushTrigger, Session, SessionMessage};
use pi::session_store_v2::SessionStoreV2;
use serde_json::json;
use std::future::Future;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn run_async<T>(future: impl Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(future)
}

#[cfg(unix)]
struct UnixModeGuard {
    path: PathBuf,
    original: Option<std::fs::Permissions>,
}

#[cfg(unix)]
impl UnixModeGuard {
    fn apply(path: &Path, mode: u32) -> Self {
        let original = std::fs::metadata(path)
            .expect("permission fixture metadata")
            .permissions();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("apply permission fixture mode");
        Self {
            path: path.to_path_buf(),
            original: Some(original),
        }
    }

    fn restore(&mut self) {
        if let Some(original) = self.original.as_ref() {
            std::fs::set_permissions(&self.path, original.clone())
                .expect("restore permission fixture mode");
            self.original = None;
        }
    }
}

#[cfg(unix)]
impl Drop for UnixModeGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            let _ = std::fs::set_permissions(&self.path, original);
        }
    }
}

#[cfg(unix)]
fn assert_permission_denied(error: &pi::Error) {
    let kind = match error {
        pi::Error::Io(io_error) => Some(io_error.kind()),
        _ => None,
    };
    assert_eq!(
        kind,
        Some(std::io::ErrorKind::PermissionDenied),
        "expected typed PermissionDenied error, got {error}"
    );
}

fn make_msg(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    }
}

fn valid_header() -> String {
    serde_json::to_string(&json!({
        "type": "session",
        "version": 3,
        "id": "fault-inject-test",
        "timestamp": "2024-06-01T00:00:00.000Z",
        "cwd": "/tmp/test"
    }))
    .unwrap()
}

fn valid_entry(id: &str, text: &str) -> String {
    json!({
        "type": "message",
        "id": id,
        "timestamp": "2024-06-01T00:00:00.000Z",
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

/// Structured trace event for fault-injection diagnostics.
#[derive(Debug)]
struct TraceEvent {
    phase: &'static str,
    action: String,
    detail: String,
}

impl TraceEvent {
    fn new(phase: &'static str, action: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            phase,
            action: action.into(),
            detail: detail.into(),
        }
    }
}

/// Trace log collector for structured diagnostics.
struct TraceLog {
    events: Vec<TraceEvent>,
}

impl TraceLog {
    const fn new() -> Self {
        Self { events: Vec::new() }
    }

    fn log(&mut self, phase: &'static str, action: impl Into<String>, detail: impl Into<String>) {
        self.events.push(TraceEvent::new(phase, action, detail));
    }

    fn dump(&self) -> String {
        self.events
            .iter()
            .map(|e| format!("[{}] {} — {}", e.phase, e.action, e.detail))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_no_errors(&self) {
        for event in &self.events {
            assert!(
                !event.detail.contains("UNEXPECTED_ERROR"),
                "Trace log contains unexpected error:\n{}",
                self.dump()
            );
        }
    }
}

// ===========================================================================
// Phase 1: Multi-phase append → crash → recover → continue → verify
// ===========================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn fault_inject_multi_phase_append_crash_recover_continue() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    // Phase 1: Create session and save initial entries.
    trace.log("SETUP", "create_session", "creating session with 5 entries");
    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());
    for i in 0..5 {
        session.append_message(make_msg(&format!("phase1-msg-{i}")));
    }
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();
    trace.log(
        "SETUP",
        "initial_save",
        format!("saved 5 entries to {}", path.display()),
    );

    // Phase 2: Append more entries incrementally.
    trace.log("APPEND", "incremental_start", "appending entries 5-9");
    for i in 5..10 {
        session.append_message(make_msg(&format!("phase2-msg-{i}")));
        run_async(async { session.save().await }).unwrap();
    }
    trace.log(
        "APPEND",
        "incremental_done",
        format!("persisted_entry_count={}", session.entries.len()),
    );

    // Phase 3: Inject crash — truncate mid-entry after the last save.
    trace.log(
        "FAULT",
        "inject_truncation",
        "appending partial JSON to simulate crash",
    );
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(
            file,
            "{{\"type\":\"message\",\"id\":\"crash-victim\",\"timestamp\""
        )
        .unwrap();
    }
    trace.log("FAULT", "truncation_injected", "partial entry appended");

    // Phase 4: Recover from crash.
    trace.log("RECOVER", "open_with_diagnostics", "attempting recovery");
    let (recovered, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();

    trace.log(
        "RECOVER",
        "diagnostics",
        format!(
            "recovered_entries={}, skipped={}, orphans={}",
            recovered.entries.len(),
            diagnostics.skipped_entries.len(),
            diagnostics.orphaned_parent_links.len(),
        ),
    );

    assert_eq!(
        recovered.entries.len(),
        10,
        "all 10 valid entries should survive crash\nTrace:\n{}",
        trace.dump()
    );
    assert_eq!(
        diagnostics.skipped_entries.len(),
        1,
        "exactly one partial entry should be skipped\nTrace:\n{}",
        trace.dump()
    );

    // Phase 5: A session recovered with skipped rows is read-only. Neither a
    // dirty-header rewrite nor an incremental append may persist it, because
    // either would commit a lossy view of the file (fail-closed contract
    // since 2026-08-27, see session.rs crash_corrupt_middle_refuses_lossy_continue).
    trace.log(
        "CONTINUE",
        "rewrite_attempt",
        "attempting a full rewrite from the recovered session",
    );
    let mut continued = recovered;
    continued.session_dir = Some(temp_dir.path().to_path_buf());
    continued.set_model_header(Some("healing-model".to_string()), None, None);
    let rewrite_error = run_async(async { continued.save().await })
        .expect_err("a recovered session with skipped rows must not be rewritten");
    assert!(
        rewrite_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected rewrite error: {rewrite_error}\nTrace:\n{}",
        trace.dump()
    );

    trace.log(
        "CONTINUE",
        "append_attempt",
        "attempting to append from the recovered session",
    );
    continued.append_message(make_msg("phase5-msg-10"));
    continued.append_message(make_msg("phase5-msg-11"));
    let append_error = run_async(async { continued.save().await })
        .expect_err("a recovered session with skipped rows must not append");
    assert!(
        append_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected append error: {append_error}\nTrace:\n{}",
        trace.dump()
    );

    // Phase 6: Final verification — the file still holds the ten valid
    // entries and the torn record; nothing was laundered or lost.
    trace.log(
        "VERIFY",
        "final_load",
        "load after the refused continuation",
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("crash-victim"),
        "the torn record must be preserved on disk\nTrace:\n{}",
        trace.dump()
    );
    let (final_session, final_diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    assert_eq!(
        final_session.entries.len(),
        10,
        "the ten valid entries survive; refused writes added nothing\nTrace:\n{}",
        trace.dump()
    );
    assert_eq!(final_diagnostics.skipped_entries.len(), 1);

    trace.log(
        "VERIFY",
        "success",
        "multi-phase fault injection test passed (lossy continuation refused)",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 2: Checkpoint heals accumulated corruption via header dirtying
// ===========================================================================

#[test]
fn fault_inject_checkpoint_heals_corruption_via_header_dirty() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    // Initial save.
    session.append_message(make_msg("initial"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();
    trace.log("SETUP", "initial_save", "1 entry saved");

    // Do several incremental appends.
    for i in 0..5 {
        session.append_message(make_msg(&format!("append-{i}")));
        run_async(async { session.save().await }).unwrap();
    }
    trace.log("APPEND", "incremental", "5 incremental appends done");

    // Inject corruption between entries.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "\nGARBAGE_PRE_CHECKPOINT\n").unwrap();
    }
    trace.log(
        "FAULT",
        "inject_corruption",
        "garbage injected between saves",
    );

    // Dirty the header to force a full rewrite on the next save. A checkpoint
    // is not allowed to launder externally corrupted rows: the rewrite
    // re-reads the file, finds rows it must skip, and refuses (fail-closed
    // contract since 2026-08-27).
    session.set_model_header(Some("checkpoint-provider".to_string()), None, None);
    session.append_message(make_msg("post-checkpoint"));
    let error = run_async(async { session.save().await })
        .expect_err("checkpoint rewrite over corrupted rows must be refused");
    assert!(
        error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected error: {error}\nTrace:\n{}",
        trace.dump()
    );
    trace.log(
        "CHECKPOINT",
        "header_dirty_rewrite_refused",
        "full rewrite via dirty header refused by the integrity guard",
    );

    // The garbage stays on disk and every valid entry stays recoverable.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("GARBAGE_PRE_CHECKPOINT"),
        "refused checkpoint must preserve the corrupt row\nTrace:\n{}",
        trace.dump()
    );

    let (loaded, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    assert_eq!(
        loaded.entries.len(),
        6,
        "1 initial + 5 incremental entries stay recoverable; the refused post-checkpoint entry was never written\nTrace:\n{}",
        trace.dump()
    );
    assert!(
        !diagnostics.skipped_entries.is_empty(),
        "the corrupt row is reported in diagnostics"
    );

    trace.log(
        "VERIFY",
        "checkpoint_refused",
        "corruption preserved and reported instead of healed",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 3: Stale temp file detection after interrupted atomic rewrite
// ===========================================================================

#[test]
fn fault_inject_stale_temp_file_after_interrupted_rewrite() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_msg("original-entry"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();
    trace.log(
        "SETUP",
        "initial_save",
        format!("saved to {}", path.display()),
    );

    // Simulate stale temp file left by interrupted atomic rewrite.
    let parent = path.parent().unwrap();
    let stale_temp = parent.join(".tmp_session_interrupted_XXXXXX");
    std::fs::write(&stale_temp, "STALE PARTIAL REWRITE CONTENT").unwrap();
    trace.log(
        "FAULT",
        "create_stale_temp",
        format!("created stale temp at {}", stale_temp.display()),
    );

    // Normal operation should succeed despite stale temp file.
    session.append_message(make_msg("after-stale-temp"));
    run_async(async { session.save().await }).unwrap();

    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

    assert_eq!(
        loaded.entries.len(),
        2,
        "session should load correctly despite stale temp file\nTrace:\n{}",
        trace.dump()
    );

    // Verify the stale temp file hasn't been touched (our save uses different naming).
    assert!(
        stale_temp.exists(),
        "stale temp file should still exist (not our responsibility to clean)"
    );

    trace.log(
        "VERIFY",
        "stale_temp_isolated",
        "stale temp file did not interfere",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 4: Autosave queue state machine under fault injection
// ===========================================================================

#[cfg(unix)]
#[test]
fn fault_inject_autosave_queue_mutation_tracking_through_faults() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    // Enqueue mutations without flushing.
    for i in 0..3 {
        session.append_message(make_msg(&format!("queued-{i}")));
    }
    let metrics_before = session.autosave_metrics();
    trace.log(
        "QUEUE",
        "mutations_enqueued",
        format!(
            "pending={}, coalesced={}",
            metrics_before.pending_mutations, metrics_before.coalesced_mutations,
        ),
    );

    // First flush — should succeed.
    run_async(async { session.flush_autosave(AutosaveFlushTrigger::Periodic).await }).unwrap();
    let path = session.path.clone().unwrap();
    let metrics_after_flush = session.autosave_metrics();
    trace.log(
        "QUEUE",
        "first_flush",
        format!(
            "pending={}, succeeded={}, batch_size={}",
            metrics_after_flush.pending_mutations,
            metrics_after_flush.flush_succeeded,
            metrics_after_flush.last_flush_batch_size,
        ),
    );

    assert_eq!(metrics_after_flush.flush_succeeded, 1);
    assert_eq!(metrics_after_flush.pending_mutations, 0);

    // Enqueue more mutations then force a save failure.
    session.append_message(make_msg("will-fail"));
    // Simulate a real append failure. Session persistence honors explicit Unix
    // mode bits even for UID 0, so the same fault reaches every test identity.
    let mut mode_guard = UnixModeGuard::apply(&path, 0o444);
    trace.log("FAULT", "make_readonly", "set session file to read-only");

    // Attempt save — should fail.
    let result = run_async(async { session.save().await });
    trace.log(
        "FAULT",
        "save_after_readonly",
        format!("result: {}", if result.is_ok() { "ok" } else { "err" }),
    );

    mode_guard.restore();
    trace.log(
        "RECOVER",
        "restore_permissions",
        "restored write permissions",
    );

    let error = result.expect_err("save of a mode-0444 session must fail");
    assert_permission_denied(&error);
    let metrics_after_fault = session.autosave_metrics();
    assert_eq!(metrics_after_fault.flush_failed, 1);
    assert!(metrics_after_fault.pending_mutations > 0);

    // Retry save — should succeed now.
    let result = run_async(async { session.save().await });
    assert!(
        result.is_ok(),
        "save should succeed after permission fix\nTrace:\n{}",
        trace.dump()
    );

    let final_metrics = session.autosave_metrics();
    trace.log(
        "VERIFY",
        "final_metrics",
        format!(
            "succeeded={}, failed={}, pending={}",
            final_metrics.flush_succeeded,
            final_metrics.flush_failed,
            final_metrics.pending_mutations,
        ),
    );
    assert_eq!(final_metrics.flush_succeeded, 2);
    assert_eq!(final_metrics.flush_failed, 1);
    assert_eq!(final_metrics.pending_mutations, 0);

    // Verify full round-trip.
    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(
        loaded.entries.len(),
        4,
        "all entries should survive permission fault cycle\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 5: Durability mode fault behavior matrix
// ===========================================================================

#[cfg(unix)]
#[test]
fn fault_inject_first_save_denial_leaves_no_partial_session_tree() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let sessions_root = temp_dir.path().join("sessions");
    std::fs::create_dir(&sessions_root).expect("create sessions root");

    let mut session = Session::create();
    session.session_dir = Some(sessions_root.clone());
    session.append_message(make_msg("pending-first-save"));

    // The fixture owner lacks write while group/other are writable. The same
    // strict assertion therefore exercises effective-class policy under both
    // UID 0 and UID 1000, with RAII restoration and no conditional skip.
    let mut mode_guard = UnixModeGuard::apply(&sessions_root, 0o577);
    let result = run_async(async { session.save().await });
    mode_guard.restore();

    let error = result.expect_err("first save must fail before directory creation");
    assert_permission_denied(&error);
    assert!(session.path.is_none(), "failed save must not assign a path");
    assert_eq!(
        session.entries.len(),
        1,
        "pending entry must remain in memory"
    );
    assert_eq!(
        std::fs::read_dir(&sessions_root)
            .expect("read restored sessions root")
            .count(),
        0,
        "permission denial must leave no partial project directory"
    );
}

#[test]
fn fault_inject_durability_strict_fails_on_io_error() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.set_autosave_durability_mode(AutosaveDurabilityMode::Strict);
    // Point at non-existent nested directory.
    session.path = Some(
        temp_dir
            .path()
            .join("nonexistent")
            .join("deep")
            .join("session.jsonl"),
    );
    session.append_message(make_msg("strict-entry"));

    let result = run_async(async { session.save().await });
    trace.log(
        "STRICT",
        "save_to_missing_dir",
        format!("result: {:?}", result.is_err()),
    );

    assert!(
        result.is_err(),
        "strict mode: save to missing dir must fail\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

#[test]
fn fault_inject_durability_balanced_swallows_io_error() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.set_autosave_durability_mode(AutosaveDurabilityMode::Balanced);
    session.path = Some(temp_dir.path().join("nonexistent").join("session.jsonl"));
    session.append_message(make_msg("balanced-entry"));

    // Balanced shutdown should not propagate errors.
    let result = run_async(async { session.flush_autosave_on_shutdown().await });
    trace.log(
        "BALANCED",
        "shutdown_flush",
        format!("result: {:?}", result.is_ok()),
    );

    assert!(
        result.is_ok(),
        "balanced mode: shutdown should swallow IO errors\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

#[test]
fn fault_inject_durability_throughput_skips_entirely() {
    let mut trace = TraceLog::new();

    let mut session = Session::create();
    session.set_autosave_durability_mode(AutosaveDurabilityMode::Throughput);
    // Deliberately point at an impossible path.
    session.path = Some(PathBuf::from("/impossible/path/session.jsonl"));
    session.append_message(make_msg("throughput-entry"));

    let result = run_async(async { session.flush_autosave_on_shutdown().await });
    trace.log(
        "THROUGHPUT",
        "shutdown_flush",
        format!("result: {:?}", result.is_ok()),
    );

    assert!(
        result.is_ok(),
        "throughput mode: shutdown should skip flush entirely\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 6: Rapid append-crash-recover cycles (stress test)
// ===========================================================================

#[test]
fn fault_inject_rapid_crash_recover_cycles() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    // Ten independent crash/recover cycles, one session file each: once a
    // file carries a torn fragment, every further write to it is refused
    // (fail-closed contract since 2026-08-27), so a cycle cannot continue on
    // the same file. What must hold in every cycle: recovery finds every
    // valid entry, reports the fragment, and refuses to persist from the
    // recovered copy; the fragment stays on disk.
    for cycle in 0..10 {
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());
        session.append_message(make_msg("seed"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // Add entries.
        let new_count = (cycle % 3) + 1;
        for j in 0..new_count {
            session.append_message(make_msg(&format!("cycle{cycle}-msg{j}")));
        }
        run_async(async { session.save().await }).unwrap();
        let expected_entries = 1 + new_count;
        trace.log(
            "CYCLE",
            format!("start_{cycle}"),
            format!("entries={expected_entries}"),
        );

        // Inject corruption (simulating a crash during the next append).
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            write!(file, "{{\"broken\":\"crash-{cycle}\"").unwrap();
        }
        trace.log("FAULT", format!("inject_{cycle}"), "partial JSON injected");

        // Recover.
        let (recovered, diag) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(
            recovered.entries.len(),
            expected_entries,
            "cycle {cycle}: expected {expected_entries} entries, got {}\nTrace:\n{}",
            recovered.entries.len(),
            trace.dump(),
        );
        assert_eq!(
            diag.skipped_entries.len(),
            1,
            "cycle {cycle}: the injected fragment is reported once"
        );

        // The recovered copy is read-only.
        let mut recovered = recovered;
        recovered.session_dir = Some(temp_dir.path().to_path_buf());
        recovered.set_model_header(Some(format!("provider-cycle-{cycle}")), None, None);
        let error = run_async(async { recovered.save().await })
            .expect_err("a recovered session with skipped rows must not persist");
        assert!(
            error
                .to_string()
                .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
            "cycle {cycle}: unexpected error: {error}\nTrace:\n{}",
            trace.dump(),
        );

        // The healthy writer is refused too: the file itself is tainted.
        session.append_message(make_msg(&format!("cycle{cycle}-after-crash")));
        let error = run_async(async { session.save().await })
            .expect_err("appending to a file with a torn fragment must be refused");
        assert!(
            error
                .to_string()
                .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
            "cycle {cycle}: unexpected append error: {error}\nTrace:\n{}",
            trace.dump(),
        );

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(&format!("crash-{cycle}")),
            "cycle {cycle}: the torn fragment must stay on disk\nTrace:\n{}",
            trace.dump(),
        );

        trace.log(
            "RECOVER",
            format!("refused_{cycle}"),
            format!(
                "entries={}, writes refused, fragment preserved",
                recovered.entries.len()
            ),
        );
    }

    trace.log(
        "VERIFY",
        "all_cycles_passed",
        "10 independent crash/recover cycles verified",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 7: Header dirty flag forces clean rewrite over corrupted file
// ===========================================================================

#[test]
fn fault_inject_header_dirty_forces_clean_rewrite() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    // Save initial entries.
    for i in 0..3 {
        session.append_message(make_msg(&format!("msg-{i}")));
    }
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Read file to get baseline.
    let baseline = std::fs::read_to_string(&path).unwrap();
    let baseline_lines = baseline.lines().count();
    trace.log("SETUP", "baseline", format!("lines={baseline_lines}"));

    // Inject corruption into the file (simulating partial append crash).
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "CORRUPTION_LINE_1\nCORRUPTION_LINE_2\n").unwrap();
    }
    trace.log("FAULT", "inject_corruption", "2 garbage lines appended");

    // Dirty the header — this forces a full rewrite on the next save. The
    // rewrite re-reads the file, sees rows it had to skip, and refuses: a
    // full rewrite would silently launder the corruption away (fail-closed
    // contract since 2026-08-27, see session.rs
    // crash_disk_corruption_after_clean_open_blocks_full_rewrite).
    session.set_model_header(Some("dirty-provider".to_string()), None, None);
    session.append_message(make_msg("after-corruption"));
    trace.log(
        "DIRTY",
        "header_dirtied",
        "model header changed, forcing full rewrite",
    );

    let error = run_async(async { session.save().await })
        .expect_err("a full rewrite over externally corrupted rows must be refused");
    assert!(
        error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected error: {error}\nTrace:\n{}",
        trace.dump()
    );

    // The corrupt bytes are preserved for inspection, not rewritten away.
    let preserved = std::fs::read_to_string(&path).unwrap();
    assert!(
        preserved.contains("CORRUPTION_LINE_1") && preserved.contains("CORRUPTION_LINE_2"),
        "refused rewrite must leave the corrupt rows in place\nTrace:\n{}",
        trace.dump()
    );

    let (loaded, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    assert_eq!(
        loaded.entries.len(),
        3,
        "the three entries persisted before the corruption stay recoverable"
    );
    assert_eq!(
        diagnostics.skipped_entries.len(),
        2,
        "both corrupt rows are reported, not hidden"
    );

    trace.log(
        "VERIFY",
        "corruption_preserved",
        "rewrite refused; corrupt rows preserved and reported",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 8: V2 store segment/index consistency after fault injection
// ===========================================================================

#[test]
fn fault_inject_v2_store_segment_corruption_recovery() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let store_root = temp_dir.path().join("v2-fault-test");

    // Create V2 store and append entries.
    let mut store = SessionStoreV2::create(&store_root, 4096).unwrap();
    trace.log(
        "SETUP",
        "v2_store_created",
        format!("root={}", store_root.display()),
    );

    for i in 0..5 {
        let payload = json!({
            "content": format!("v2 message {i}")
        });
        store
            .append_entry(format!("v2-entry-{i}"), None, "message", payload)
            .unwrap();
    }
    trace.log("SETUP", "entries_appended", "5 entries to V2 store");

    // Read all entries to verify baseline.
    let all = store.read_all_entries().unwrap();
    assert_eq!(all.len(), 5, "baseline: 5 entries");

    // Inject corruption into the active segment file.
    let seg_path = store.segment_file_path(1);
    trace.log(
        "FAULT",
        "corrupt_segment",
        format!("appending garbage to {}", seg_path.display()),
    );
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&seg_path)
            .unwrap();
        // Write a partial frame header (not enough bytes for a valid frame).
        file.write_all(&[0xFF, 0xFE, 0xFD, 0xFC, 0x00]).unwrap();
    }

    // Reading entries should still work — the store should handle trailing corruption.
    let after_corruption = store.read_all_entries();
    trace.log(
        "RECOVER",
        "read_after_corruption",
        format!("result: {:?}", after_corruption.as_ref().map(Vec::len)),
    );

    // Whether read_all_entries succeeds or fails, the valid entries before corruption
    // should be accessible via index lookup.
    let entry0 = store.lookup_entry(0);
    trace.log(
        "VERIFY",
        "lookup_entry_0",
        format!("result: {:?}", entry0.is_ok()),
    );

    // Create checkpoint to snapshot known-good state.
    let checkpoint = store.create_checkpoint(1, "recovery");
    trace.log(
        "CHECKPOINT",
        "create",
        format!("result: {:?}", checkpoint.is_ok()),
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 9: Save to read-only directory simulating filesystem error
// ===========================================================================

#[cfg(unix)]
#[test]
fn fault_inject_save_to_readonly_filesystem() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());
    session.append_message(make_msg("first-save"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();
    trace.log("SETUP", "initial_save", "1 entry saved");

    // Make the parent directory non-writable. Session persistence honors the
    // explicit mode bits even for UID 0, keeping this fault deterministic.
    let parent = path.parent().unwrap();
    let mut mode_guard = UnixModeGuard::apply(parent, 0o555);
    trace.log("FAULT", "make_parent_readonly", "directory set to r-x");

    // Force full rewrite by dirtying header.
    session.set_model_header(Some("test".to_string()), None, None);
    session.append_message(make_msg("will-fail-save"));
    let result = run_async(async { session.save().await });
    trace.log(
        "FAULT",
        "save_to_readonly",
        format!("result: {}", if result.is_ok() { "ok" } else { "err" }),
    );

    mode_guard.restore();
    trace.log(
        "RECOVER",
        "restore_permissions",
        "directory permissions restored",
    );

    let error = result.expect_err("full rewrite in a mode-0555 directory must fail");
    assert_permission_denied(&error);

    // Atomic rewrite failure must leave the original file intact.
    let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
    assert_eq!(
        loaded.entries.len(),
        1,
        "unexpected persisted entry count after read-only fault\nTrace:\n{}",
        trace.dump()
    );

    trace.log(
        "VERIFY",
        "original_intact",
        "atomic rewrite failure preserved original file",
    );
    trace.assert_no_errors();
}

// ===========================================================================
// Phase 10: Mixed entry types survive fault injection
// ===========================================================================

#[test]
fn fault_inject_mixed_entry_types_through_crash_cycle() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    // Add diverse entry types.
    let msg_id = session.append_message(make_msg("user message"));
    session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());
    session.append_thinking_level_change("high".to_string());
    session.append_compaction(
        "summary of earlier conversation".to_string(),
        msg_id,
        500,
        None,
        None,
    );
    session.append_message(make_msg("after compaction"));

    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();
    trace.log(
        "SETUP",
        "diverse_entries",
        format!("{} entries saved", session.entries.len()),
    );

    // Inject crash.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{{\"type\":\"message\",\"id\":\"victim\"").unwrap();
    }
    trace.log("FAULT", "inject_partial", "partial entry appended");

    // Recover.
    let (recovered, diag) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();

    trace.log(
        "RECOVER",
        "diagnostics",
        format!(
            "recovered={}, skipped={}",
            recovered.entries.len(),
            diag.skipped_entries.len(),
        ),
    );

    assert_eq!(
        recovered.entries.len(),
        5,
        "all 5 diverse entries should survive\nTrace:\n{}",
        trace.dump()
    );
    assert_eq!(diag.skipped_entries.len(), 1);

    // The recovered session is read-only: rewriting or appending from it
    // would commit a lossy view of the file (fail-closed contract since
    // 2026-08-27), so both are refused and the diverse entries stay intact.
    let mut cont = recovered;
    cont.session_dir = Some(temp_dir.path().to_path_buf());
    cont.set_model_header(Some("healing-model".to_string()), None, None);
    let rewrite_error = run_async(async { cont.save().await })
        .expect_err("a recovered session with skipped rows must not be rewritten");
    assert!(
        rewrite_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected rewrite error: {rewrite_error}\nTrace:\n{}",
        trace.dump()
    );

    cont.append_message(make_msg("post-recovery"));
    let append_error = run_async(async { cont.save().await })
        .expect_err("a recovered session with skipped rows must not append");
    assert!(
        append_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected append error: {append_error}\nTrace:\n{}",
        trace.dump()
    );

    let (final_load, final_diag) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    assert_eq!(
        final_load.entries.len(),
        5,
        "the 5 diverse entries survive; refused writes added nothing\nTrace:\n{}",
        trace.dump()
    );
    assert_eq!(final_diag.skipped_entries.len(), 1);

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 11: Corruption healed by header-dirty checkpoint rewrite
// ===========================================================================

#[test]
fn fault_inject_corruption_healed_at_checkpoint() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    session.append_message(make_msg("seed-entry"));
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Inject corruption.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "\nGARBAGE_WILL_BE_HEALED\n").unwrap();
    }
    trace.log("FAULT", "inject_garbage", "garbage injected between saves");

    // Every write after external corruption is refused: the incremental
    // append re-validates the on-disk tail and finds a row it would have to
    // skip, and a dirty-header checkpoint would launder that row away. Both
    // fail closed (contract since 2026-08-27; see session.rs
    // crash_disk_corruption_after_clean_open_blocks_full_rewrite).
    session.append_message(make_msg("incremental-0"));
    let append_error = run_async(async { session.save().await })
        .expect_err("an incremental append over a corrupt row must be refused");
    assert!(
        append_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected append error: {append_error}\nTrace:\n{}",
        trace.dump()
    );
    trace.log(
        "APPEND",
        "refused",
        "incremental append refused by the integrity guard",
    );

    session.set_model_header(Some("force-checkpoint".to_string()), None, None);
    let checkpoint_error = run_async(async { session.save().await })
        .expect_err("a checkpoint rewrite over a corrupt row must be refused");
    assert!(
        checkpoint_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected checkpoint error: {checkpoint_error}\nTrace:\n{}",
        trace.dump()
    );
    trace.log(
        "CHECKPOINT",
        "refused",
        "checkpoint via dirty header refused by the integrity guard",
    );

    // The garbage stays on disk and the seed entry stays recoverable.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("GARBAGE_WILL_BE_HEALED"),
        "refused writes must preserve the corrupt row\nTrace:\n{}",
        trace.dump()
    );

    let (loaded, diagnostics) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    assert_eq!(
        loaded.entries.len(),
        1,
        "the seed entry stays recoverable; the refused writes added nothing\nTrace:\n{}",
        trace.dump()
    );
    assert!(
        !diagnostics.skipped_entries.is_empty(),
        "the corrupt row is reported in diagnostics"
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 12: Orphaned parent link recovery
// ===========================================================================

#[test]
fn fault_inject_orphaned_parent_links_detected() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("orphan_test.jsonl");

    // Build a file with entries referencing a non-existent parent.
    let lines = [
        valid_header(),
        valid_entry("root-1", "first message"),
        // This entry references a parent that doesn't exist.
        json!({
            "type": "message",
            "id": "orphan-child",
            "parent": "nonexistent-parent",
            "timestamp": "2024-06-01T00:00:00.000Z",
            "message": {"role": "user", "content": "orphaned child"}
        })
        .to_string(),
        valid_entry("root-2", "second message"),
    ];

    std::fs::write(&file_path, lines.join("\n")).unwrap();
    trace.log(
        "SETUP",
        "orphan_file",
        "file with orphaned parent link created",
    );

    let (session, diag) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    trace.log(
        "RECOVER",
        "diagnostics",
        format!(
            "entries={}, skipped={}, orphans={}",
            session.entries.len(),
            diag.skipped_entries.len(),
            diag.orphaned_parent_links.len(),
        ),
    );

    // All entries should load (orphaned links are noted, not fatal).
    assert!(
        session.entries.len() >= 2,
        "at least the non-orphaned entries should load\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 13: Save idempotency after fault-recovery round-trip
// ===========================================================================

#[test]
fn fault_inject_save_idempotency_after_recovery() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();

    let mut session = Session::create();
    session.session_dir = Some(temp_dir.path().to_path_buf());

    for i in 0..5 {
        session.append_message(make_msg(&format!("msg-{i}")));
    }
    run_async(async { session.save().await }).unwrap();
    let path = session.path.clone().unwrap();

    // Inject and recover.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{{\"broken").unwrap();
    }

    let (mut recovered, _) =
        run_async(async { Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await })
            .unwrap();
    recovered.session_dir = Some(temp_dir.path().to_path_buf());

    // A session recovered with a skipped row is read-only (fail-closed
    // contract since 2026-08-27): every save attempt is refused, and the
    // refusal is idempotent — the file bytes never change.
    let content_before = std::fs::read_to_string(&path).unwrap();
    let first_error = run_async(async { recovered.save().await })
        .expect_err("a recovered session with skipped rows must not persist");
    assert!(
        first_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected first error: {first_error}\nTrace:\n{}",
        trace.dump()
    );
    let content_after_first = std::fs::read_to_string(&path).unwrap();
    trace.log(
        "IDEMPOTENCY",
        "first_save_refused",
        format!("size={}", content_after_first.len()),
    );

    let second_error =
        run_async(async { recovered.save().await }).expect_err("the refusal must hold on retry");
    assert!(
        second_error
            .to_string()
            .contains("PI_SESSION_SOURCE_INTEGRITY_FAILED"),
        "unexpected second error: {second_error}\nTrace:\n{}",
        trace.dump()
    );
    let content_after_second = std::fs::read_to_string(&path).unwrap();
    trace.log(
        "IDEMPOTENCY",
        "second_save_refused",
        format!("size={}", content_after_second.len()),
    );

    assert_eq!(
        content_before,
        content_after_first,
        "a refused save must not touch the file\nTrace:\n{}",
        trace.dump()
    );
    assert_eq!(
        content_after_first,
        content_after_second,
        "the refusal is idempotent\nTrace:\n{}",
        trace.dump()
    );

    trace.assert_no_errors();
}

// ===========================================================================
// Phase 14: Large session recovery under scattered corruption
// ===========================================================================

#[test]
fn fault_inject_large_session_scattered_corruption() {
    let mut trace = TraceLog::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("large_corrupt.jsonl");

    // Build a large session with scattered corruption.
    let mut lines = vec![valid_header()];
    let mut valid_count = 0;
    for i in 0..100 {
        if i % 17 == 0 && i > 0 {
            // Every 17th entry is corrupted.
            lines.push(format!("CORRUPTION_AT_LINE_{i}"));
        } else {
            lines.push(valid_entry(&format!("entry-{i}"), &format!("message {i}")));
            valid_count += 1;
        }
    }
    std::fs::write(&file_path, lines.join("\n")).unwrap();
    trace.log(
        "SETUP",
        "large_session",
        format!(
            "100 lines, {} valid, {} corrupted",
            valid_count,
            100 - valid_count
        ),
    );

    let (session, diag) = run_async(async {
        Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
    })
    .unwrap();

    trace.log(
        "RECOVER",
        "large_session_loaded",
        format!(
            "entries={}, skipped={}",
            session.entries.len(),
            diag.skipped_entries.len(),
        ),
    );

    assert_eq!(
        session.entries.len(),
        valid_count,
        "exactly {valid_count} valid entries should survive\nTrace:\n{}",
        trace.dump()
    );
    assert!(
        !diag.skipped_entries.is_empty(),
        "some entries should be skipped"
    );

    // Verify diagnostics include line numbers.
    for skip in &diag.skipped_entries {
        trace.log(
            "DIAGNOSTIC",
            "skipped_entry",
            format!("line={}, error={}", skip.line_number, skip.error),
        );
    }

    trace.assert_no_errors();
}
