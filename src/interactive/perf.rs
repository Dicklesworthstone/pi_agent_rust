use std::collections::VecDeque;
use std::sync::Arc;

use asupersync::sync::OwnedMutexGuard;
use serde_json::{Value, json};

use super::{AgentState, Cmd, EXTENSION_EVENT_TIMEOUT_MS, PiApp, PiMsg, conversation_from_session};
use crate::extension_events::{SessionBeforeCompactOutcome, apply_session_before_compact_response};

/// Safely convert `Duration::as_micros()` (u128) to u64 with saturation.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn micros_as_u64(micros: u128) -> u64 {
    micros.min(u128::from(u64::MAX)) as u64
}

/// Microsecond-precision frame timing stats for TUI performance measurement.
///
/// Uses interior mutability (`RefCell`/`Cell`) because `view(&self)` cannot
/// take `&mut self` (the `bubbletea::Model` trait requires `&self` for `view`).
/// This is safe because the TUI event loop is single-threaded.
///
/// Gated behind `PI_PERF_TELEMETRY=1` environment variable.  When disabled,
/// no `Instant::now()` calls are made — zero runtime overhead.
pub struct FrameTimingStats {
    pub(super) frame_times_us: std::cell::RefCell<VecDeque<u64>>,
    pub(super) content_build_times_us: std::cell::RefCell<VecDeque<u64>>,
    pub(super) viewport_sync_times_us: std::cell::RefCell<VecDeque<u64>>,
    pub(super) update_times_us: VecDeque<u64>,
    pub(super) total_frames: std::cell::Cell<u64>,
    pub(super) budget_exceeded_count: std::cell::Cell<u64>,
    pub(super) enabled: bool,
}

pub(super) const FRAME_TIMING_WINDOW: usize = 60;
pub(super) const FRAME_BUDGET_US: u64 = 16_667;

impl FrameTimingStats {
    pub(super) fn new() -> Self {
        let enabled =
            std::env::var_os("PI_PERF_TELEMETRY").is_some_and(|v| v == "1" || v == "true");
        Self {
            frame_times_us: std::cell::RefCell::new(VecDeque::with_capacity(FRAME_TIMING_WINDOW)),
            content_build_times_us: std::cell::RefCell::new(VecDeque::with_capacity(
                FRAME_TIMING_WINDOW,
            )),
            viewport_sync_times_us: std::cell::RefCell::new(VecDeque::with_capacity(
                FRAME_TIMING_WINDOW,
            )),
            update_times_us: VecDeque::with_capacity(FRAME_TIMING_WINDOW),
            total_frames: std::cell::Cell::new(0),
            budget_exceeded_count: std::cell::Cell::new(0),
            enabled,
        }
    }

    pub(super) fn record_frame(&self, elapsed_us: u64) {
        if !self.enabled {
            return;
        }
        let metrics = crate::session_metrics::global();
        if metrics.enabled() {
            metrics.tui_render.record(elapsed_us);
        }
        let mut times = self.frame_times_us.borrow_mut();
        if times.len() >= FRAME_TIMING_WINDOW {
            times.pop_front();
        }
        times.push_back(elapsed_us);
        let total = self.total_frames.get() + 1;
        self.total_frames.set(total);
        if elapsed_us > FRAME_BUDGET_US {
            self.budget_exceeded_count
                .set(self.budget_exceeded_count.get() + 1);
        }
        if total.is_multiple_of(FRAME_TIMING_WINDOW as u64) {
            drop(times);
            self.emit_stats();
        }
    }

    pub(super) fn record_content_build(&self, elapsed_us: u64) {
        if !self.enabled {
            return;
        }
        let metrics = crate::session_metrics::global();
        if metrics.enabled() {
            metrics.tui_content_build.record(elapsed_us);
        }
        let mut times = self.content_build_times_us.borrow_mut();
        if times.len() >= FRAME_TIMING_WINDOW {
            times.pop_front();
        }
        times.push_back(elapsed_us);
    }

    pub(super) fn record_viewport_sync(&self, elapsed_us: u64) {
        if !self.enabled {
            return;
        }
        let metrics = crate::session_metrics::global();
        if metrics.enabled() {
            metrics.tui_viewport_sync.record(elapsed_us);
        }
        let mut times = self.viewport_sync_times_us.borrow_mut();
        if times.len() >= FRAME_TIMING_WINDOW {
            times.pop_front();
        }
        times.push_back(elapsed_us);
    }

    pub(super) fn record_update(&mut self, elapsed_us: u64) {
        if !self.enabled {
            return;
        }
        let metrics = crate::session_metrics::global();
        if metrics.enabled() {
            metrics.tui_update.record(elapsed_us);
        }
        if self.update_times_us.len() >= FRAME_TIMING_WINDOW {
            self.update_times_us.pop_front();
        }
        self.update_times_us.push_back(elapsed_us);
    }

    pub(super) fn frame_p99_us(&self) -> u64 {
        Self::percentiles(&self.frame_times_us.borrow()).2
    }

    pub(super) const fn enable_for_test(&mut self) {
        self.enabled = true;
    }

    pub(super) fn reset_for_test(&mut self) {
        self.frame_times_us.borrow_mut().clear();
        self.content_build_times_us.borrow_mut().clear();
        self.viewport_sync_times_us.borrow_mut().clear();
        self.update_times_us.clear();
        self.total_frames.set(0);
        self.budget_exceeded_count.set(0);
    }

    pub(super) fn percentiles(times: &VecDeque<u64>) -> (u64, u64, u64, u64) {
        if times.is_empty() {
            return (0, 0, 0, 0);
        }
        let mut sorted: Vec<u64> = times.iter().copied().collect();
        sorted.sort_unstable();
        let len = sorted.len();
        let p50 = sorted[len / 2];
        let p95 = sorted[(len * 95 / 100).min(len - 1)];
        let p99 = sorted[(len * 99 / 100).min(len - 1)];
        let p999 = sorted[(len * 999 / 1000).min(len - 1)];
        (p50, p95, p99, p999)
    }

    pub(super) fn snapshot_json(&self, surface: &str, fixture: &Value) -> Value {
        let frame_times = self.frame_times_us.borrow();
        let content_times = self.content_build_times_us.borrow();
        let viewport_times = self.viewport_sync_times_us.borrow();
        let update_times = &self.update_times_us;
        let recent_over_budget =
            Self::over_budget_count(frame_times.as_slices().0, FRAME_BUDGET_US)
                + Self::over_budget_count(frame_times.as_slices().1, FRAME_BUDGET_US);
        let verdict = if frame_times.is_empty() {
            "empty"
        } else if recent_over_budget == 0 {
            "pass"
        } else {
            "warn"
        };

        json!({
            "schema": "pi.tui.frame_budget.v1",
            "surface": surface,
            "enabled": self.enabled,
            "budget_us": FRAME_BUDGET_US,
            "window_capacity": FRAME_TIMING_WINDOW,
            "fixture": fixture,
            "samples": {
                "frame": Self::sample_stats_json(&frame_times, FRAME_BUDGET_US),
                "content_build": Self::sample_stats_json(&content_times, FRAME_BUDGET_US),
                "viewport_sync": Self::sample_stats_json(&viewport_times, FRAME_BUDGET_US),
                "update": Self::sample_stats_json(update_times, FRAME_BUDGET_US),
            },
            "totals": {
                "frames": self.total_frames.get(),
                "budget_exceeded": self.budget_exceeded_count.get(),
                "recent_budget_exceeded": recent_over_budget,
            },
            "verdict": verdict,
            "redaction": {
                "prompt_content": "omitted",
                "tool_payload_content": "omitted",
                "model_response_content": "omitted",
            },
        })
    }

    fn over_budget_count(times: &[u64], budget_us: u64) -> usize {
        times.iter().filter(|&&value| value > budget_us).count()
    }

    fn sample_stats_json(times: &VecDeque<u64>, budget_us: u64) -> Value {
        let (p50, p95, p99, p999) = Self::percentiles(times);
        let max = times.iter().copied().max().unwrap_or(0);
        let over_budget = Self::over_budget_count(times.as_slices().0, budget_us)
            + Self::over_budget_count(times.as_slices().1, budget_us);
        json!({
            "count": times.len(),
            "p50_us": p50,
            "p95_us": p95,
            "p99_us": p99,
            "p999_us": p999,
            "max_us": max,
            "over_budget_count": over_budget,
        })
    }

    #[allow(clippy::cast_precision_loss)]
    fn emit_stats(&self) {
        let frame = Self::percentiles(&self.frame_times_us.borrow());
        let content = Self::percentiles(&self.content_build_times_us.borrow());
        let viewport = Self::percentiles(&self.viewport_sync_times_us.borrow());
        let total = self.total_frames.get();
        let exceeded = self.budget_exceeded_count.get();
        let window = self.frame_times_us.borrow().len();
        let recent_exceeded = self
            .frame_times_us
            .borrow()
            .iter()
            .filter(|&&t| t > FRAME_BUDGET_US)
            .count();
        let fixture = json!({
            "name": "rolling_frame_window",
            "source": "PI_PERF_TELEMETRY",
            "sample_window": FRAME_TIMING_WINDOW,
        });
        let snapshot = self.snapshot_json("interactive_tui", &fixture);
        tracing::debug!(
            telemetry = %snapshot,
            "[perf] frame p50={:.1}ms p95={:.1}ms p99={:.1}ms p999={:.1}ms | \
             content p50={:.1}ms p95={:.1}ms p99={:.1}ms p999={:.1}ms | \
             viewport p50={:.1}ms p95={:.1}ms p99={:.1}ms p999={:.1}ms | \
             budget_exceeded={recent_exceeded}/{window} (total={exceeded}/{total})",
            frame.0 as f64 / 1000.0,
            frame.1 as f64 / 1000.0,
            frame.2 as f64 / 1000.0,
            frame.3 as f64 / 1000.0,
            content.0 as f64 / 1000.0,
            content.1 as f64 / 1000.0,
            content.2 as f64 / 1000.0,
            content.3 as f64 / 1000.0,
            viewport.0 as f64 / 1000.0,
            viewport.1 as f64 / 1000.0,
            viewport.2 as f64 / 1000.0,
            viewport.3 as f64 / 1000.0,
        );
    }

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn summary(&self) -> String {
        if !self.enabled {
            return String::from("Frame telemetry disabled (set PI_PERF_TELEMETRY=1 to enable)");
        }
        let frame = Self::percentiles(&self.frame_times_us.borrow());
        let content = Self::percentiles(&self.content_build_times_us.borrow());
        let viewport = Self::percentiles(&self.viewport_sync_times_us.borrow());
        let update = Self::percentiles(&self.update_times_us);
        let total = self.total_frames.get();
        let exceeded = self.budget_exceeded_count.get();
        format!(
            "Frame timing (last {FRAME_TIMING_WINDOW} frames):\n  \
             view()   p50={:.1}ms  p95={:.1}ms  p99={:.1}ms  p999={:.1}ms\n  \
             content  p50={:.1}ms  p95={:.1}ms  p99={:.1}ms  p999={:.1}ms\n  \
             viewport p50={:.1}ms  p95={:.1}ms  p99={:.1}ms  p999={:.1}ms\n  \
             update() p50={:.1}ms  p95={:.1}ms  p99={:.1}ms  p999={:.1}ms\n  \
             Budget exceeded: {exceeded}/{total} frames (>{:.1}ms)",
            frame.0 as f64 / 1000.0,
            frame.1 as f64 / 1000.0,
            frame.2 as f64 / 1000.0,
            frame.3 as f64 / 1000.0,
            content.0 as f64 / 1000.0,
            content.1 as f64 / 1000.0,
            content.2 as f64 / 1000.0,
            content.3 as f64 / 1000.0,
            viewport.0 as f64 / 1000.0,
            viewport.1 as f64 / 1000.0,
            viewport.2 as f64 / 1000.0,
            viewport.3 as f64 / 1000.0,
            update.0 as f64 / 1000.0,
            update.1 as f64 / 1000.0,
            update.2 as f64 / 1000.0,
            update.3 as f64 / 1000.0,
            FRAME_BUDGET_US as f64 / 1000.0,
        )
    }
}

/// TUI responsiveness pressure derived from frame latency and pending output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TuiPressureLevel {
    Normal,
    Elevated,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TuiPressureDecision {
    pub(super) level: TuiPressureLevel,
    pub(super) throttle_tool_updates: bool,
    pub(super) flush_interval: std::time::Duration,
    pub(super) max_pending_tool_events: usize,
    pub(super) max_pending_tool_output_bytes: usize,
}

impl TuiPressureDecision {
    pub(super) const fn normal() -> Self {
        Self {
            level: TuiPressureLevel::Normal,
            throttle_tool_updates: false,
            flush_interval: std::time::Duration::ZERO,
            max_pending_tool_events: 1,
            max_pending_tool_output_bytes: 32 * 1024,
        }
    }
}

pub(super) struct TuiPressureController;

impl TuiPressureController {
    pub(super) const ELEVATED_FRAME_P99_US: u64 = FRAME_BUDGET_US;
    pub(super) const HIGH_FRAME_P99_US: u64 = FRAME_BUDGET_US * 3;
    pub(super) const ELEVATED_TOOL_OUTPUT_BYTES: usize = 32 * 1024;
    pub(super) const HIGH_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
    pub(super) const ELEVATED_PENDING_TOOL_EVENTS: usize = 2;
    pub(super) const HIGH_PENDING_TOOL_EVENTS: usize = 8;

    pub(super) const fn decide(
        frame_p99_us: u64,
        output_bytes: usize,
        pending_tool_events: usize,
    ) -> TuiPressureDecision {
        if frame_p99_us >= Self::HIGH_FRAME_P99_US
            || output_bytes >= Self::HIGH_TOOL_OUTPUT_BYTES
            || pending_tool_events >= Self::HIGH_PENDING_TOOL_EVENTS
        {
            TuiPressureDecision {
                level: TuiPressureLevel::High,
                throttle_tool_updates: true,
                flush_interval: std::time::Duration::from_millis(160),
                max_pending_tool_events: Self::HIGH_PENDING_TOOL_EVENTS,
                max_pending_tool_output_bytes: 1024 * 1024,
            }
        } else if frame_p99_us >= Self::ELEVATED_FRAME_P99_US
            || output_bytes >= Self::ELEVATED_TOOL_OUTPUT_BYTES
            || pending_tool_events >= Self::ELEVATED_PENDING_TOOL_EVENTS
        {
            TuiPressureDecision {
                level: TuiPressureLevel::Elevated,
                throttle_tool_updates: true,
                flush_interval: std::time::Duration::from_millis(80),
                max_pending_tool_events: 4,
                max_pending_tool_output_bytes: Self::HIGH_TOOL_OUTPUT_BYTES,
            }
        } else {
            TuiPressureDecision::normal()
        }
    }
}

/// Memory pressure level based on RSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemoryLevel {
    /// RSS < 50MB — no action needed.
    Normal,
    /// 50MB <= RSS < 100MB — log warning, show in /session.
    Warning,
    /// 100MB <= RSS < 200MB — progressive tool output collapse.
    Pressure,
    /// RSS >= 200MB — truncate old messages, force degraded rendering.
    Critical,
}

impl MemoryLevel {
    pub(super) const fn from_rss_bytes(rss: usize) -> Self {
        const MB: usize = 1_000_000;
        if rss >= 200 * MB {
            Self::Critical
        } else if rss >= 100 * MB {
            Self::Pressure
        } else if rss >= 50 * MB {
            Self::Warning
        } else {
            Self::Normal
        }
    }
}

/// Abstraction for reading RSS, injectable for testing.
pub(super) trait RssReader: Send {
    fn read_rss_bytes(&self) -> Option<usize>;
}

pub(super) struct FnRssReader {
    read_fn: Box<dyn Fn() -> Option<usize> + Send>,
}

impl FnRssReader {
    pub(super) fn new(read_fn: Box<dyn Fn() -> Option<usize> + Send>) -> Self {
        Self { read_fn }
    }
}

impl RssReader for FnRssReader {
    fn read_rss_bytes(&self) -> Option<usize> {
        (self.read_fn)()
    }
}

/// Reads RSS from /proc/self/statm on Linux.
pub(super) struct ProcSelfRssReader;

/// Page size in bytes. Hardcoded to 4096 (standard for x86_64/aarch64 Linux)
/// to avoid unsafe libc::sysconf — crate uses `#![forbid(unsafe_code)]`.
const PROC_PAGE_SIZE: usize = 4096;

impl RssReader for ProcSelfRssReader {
    fn read_rss_bytes(&self) -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            // /proc/self/statm: "total_pages resident_pages shared_pages ..."
            let content = std::fs::read_to_string("/proc/self/statm").ok()?;
            let resident_pages: usize = content.split_whitespace().nth(1)?.parse().ok()?;
            Some(resident_pages * PROC_PAGE_SIZE)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

/// Hysteresis threshold: stop collapsing when RSS drops below this.
pub(super) const MEMORY_RELIEF_BYTES: usize = 80_000_000;

/// Maximum messages retained when Critical truncation triggers.
pub(super) const CRITICAL_KEEP_MESSAGES: usize = 30;

/// Memory monitor that drives progressive conversation management.
pub(super) struct MemoryMonitor {
    pub(super) reader: Box<dyn RssReader>,
    pub(super) last_sample: std::time::Instant,
    pub(super) sample_interval: std::time::Duration,
    pub(super) current_rss_bytes: usize,
    pub(super) peak_rss_bytes: usize,
    pub(super) level: MemoryLevel,
    /// Index into messages vec: next tool output to collapse.
    pub(super) next_collapse_index: usize,
    /// Whether progressive collapse is in progress.
    pub(super) collapsing: bool,
    /// When the last collapse action was performed (rate-limit to 1/sec).
    pub(super) last_collapse: std::time::Instant,
    /// Whether Critical truncation has already been applied this session.
    pub(super) truncated: bool,
}

impl MemoryMonitor {
    pub(super) fn new(reader: Box<dyn RssReader>) -> Self {
        let now = std::time::Instant::now();
        Self {
            reader,
            last_sample: now,
            sample_interval: std::time::Duration::from_secs(5),
            current_rss_bytes: 0,
            peak_rss_bytes: 0,
            level: MemoryLevel::Normal,
            next_collapse_index: 0,
            collapsing: false,
            last_collapse: now,
            truncated: false,
        }
    }

    pub(super) fn new_with_reader_fn(read_fn: Box<dyn Fn() -> Option<usize> + Send>) -> Self {
        Self::new(Box::new(FnRssReader::new(read_fn)))
    }

    pub(super) fn new_default() -> Self {
        Self::new(Box::new(ProcSelfRssReader))
    }

    /// Sample RSS if the interval has elapsed. Returns true if level changed.
    pub(super) fn maybe_sample(&mut self) -> bool {
        if self.last_sample.elapsed() < self.sample_interval {
            return false;
        }
        self.last_sample = std::time::Instant::now();
        let Some(rss) = self.reader.read_rss_bytes() else {
            return false;
        };
        self.current_rss_bytes = rss;
        if rss > self.peak_rss_bytes {
            self.peak_rss_bytes = rss;
        }
        let new_level = MemoryLevel::from_rss_bytes(rss);
        let changed = new_level != self.level;
        if changed {
            match new_level {
                MemoryLevel::Warning => {
                    tracing::warn!(
                        rss_mb = rss / 1_000_000,
                        "Memory pressure: Warning level reached"
                    );
                }
                MemoryLevel::Pressure => {
                    tracing::warn!(
                        rss_mb = rss / 1_000_000,
                        "Memory pressure: Pressure level — starting progressive collapse"
                    );
                    self.collapsing = true;
                }
                MemoryLevel::Critical => {
                    tracing::error!(
                        rss_mb = rss / 1_000_000,
                        "Memory pressure: Critical level — truncating conversation"
                    );
                }
                MemoryLevel::Normal => {
                    tracing::info!(
                        rss_mb = rss / 1_000_000,
                        "Memory pressure relieved — back to Normal"
                    );
                    self.collapsing = false;
                }
            }
            self.level = new_level;
        }
        changed
    }

    /// Re-sample RSS immediately (used after collapse actions).
    pub(super) fn resample_now(&mut self) {
        if let Some(rss) = self.reader.read_rss_bytes() {
            self.current_rss_bytes = rss;
            if rss > self.peak_rss_bytes {
                self.peak_rss_bytes = rss;
            }
            self.level = MemoryLevel::from_rss_bytes(rss);
            if rss < MEMORY_RELIEF_BYTES {
                self.collapsing = false;
            }
        }
    }

    /// Format memory stats for /session display.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn summary(&self) -> String {
        let current_mb = self.current_rss_bytes as f64 / 1_000_000.0;
        let peak_mb = self.peak_rss_bytes as f64 / 1_000_000.0;
        let level_str = match self.level {
            MemoryLevel::Normal => "Normal",
            MemoryLevel::Warning => "Warning",
            MemoryLevel::Pressure => "Pressure (collapsing old outputs...)",
            MemoryLevel::Critical => "CRITICAL",
        };
        format!("Memory: {current_mb:.1}MB (peak {peak_mb:.1}MB) [{level_str}]")
    }

    /// Whether Critical-level rendering degradation should be forced.
    #[cfg(test)]
    pub(super) const fn should_force_degraded(&self) -> bool {
        matches!(self.level, MemoryLevel::Critical)
    }
}

impl PiApp {
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_compact(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot compact while processing".to_string());
            return None;
        }

        let Ok(agent_guard) = self.agent.try_lock() else {
            self.status_message = Some("Agent busy; try again".to_string());
            return None;
        };
        let provider = agent_guard.provider();
        let api_key_opt = agent_guard.stream_options().api_key.clone();
        drop(agent_guard);

        let Some(api_key) = api_key_opt else {
            self.status_message = Some("No API key configured; cannot run compaction".to_string());
            return None;
        };

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let agent = Arc::clone(&self.agent);
        let extensions = self.extensions.clone();
        let runtime_handle = self.runtime_handle.clone();
        let reserve_tokens = self.config.compaction_reserve_tokens();
        let keep_recent_tokens = self.config.compaction_keep_recent_tokens();
        let custom_instructions = args.trim().to_string();
        let custom_instructions = if custom_instructions.is_empty() {
            None
        } else {
            Some(custom_instructions)
        };
        let is_compacting = Arc::clone(&self.extension_compacting);

        self.agent_state = AgentState::Processing;
        self.status_message = Some("Compacting session...".to_string());
        self.extension_compacting
            .store(true, std::sync::atomic::Ordering::SeqCst);

        runtime_handle.spawn(async move {
            let cx = asupersync::Cx::for_request();

            let path_entries = {
                let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                guard.ensure_entry_ids();
                guard
                    .entries_for_current_path()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            };

            let settings = crate::compaction::ResolvedCompactionSettings {
                enabled: true,
                reserve_tokens,
                keep_recent_tokens,
                ..Default::default()
            };
            let Some(prep) = crate::compaction::prepare_compaction(&path_entries, settings) else {
                is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System(
                        "Nothing to compact (already compacted or too little history)".to_string(),
                    ),
                )
                .await;
                return;
            };

            let before_outcome = if let Some(manager) = extensions.clone() {
                let prep_value = crate::compaction::compaction_preparation_to_value(&prep);
                let branch_entries_value =
                    serde_json::to_value(&path_entries).unwrap_or(Value::Array(Vec::new()));
                let mut payload = serde_json::Map::new();
                payload.insert("preparation".to_string(), prep_value);
                payload.insert("branchEntries".to_string(), branch_entries_value);
                if let Some(custom_instructions) = custom_instructions.as_deref() {
                    payload.insert(
                        "customInstructions".to_string(),
                        Value::String(custom_instructions.to_string()),
                    );
                }

                let response = manager
                    .dispatch_event_with_response(
                        crate::extensions::ExtensionEventName::SessionBeforeCompact,
                        Some(Value::Object(payload)),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(None);
                apply_session_before_compact_response(response, prep.tokens_before)
            } else {
                SessionBeforeCompactOutcome::default()
            };

            if before_outcome.cancel {
                is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System("Compaction cancelled by extension".to_string()),
                )
                .await;
                return;
            }

            let (summary, first_kept_entry_id, tokens_before, details, from_extension) =
                if let Some(compaction) = before_outcome.compaction {
                    (
                        compaction.summary,
                        compaction.first_kept_entry_id,
                        compaction.tokens_before,
                        compaction.details,
                        true,
                    )
                } else {
                    let result = match crate::compaction::compact(
                        prep,
                        Arc::clone(&provider),
                        &api_key,
                        custom_instructions.as_deref(),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(err) => {
                            is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &cx,
                                PiMsg::AgentError(format!("Compaction failed: {err}")),
                            )
                            .await;
                            return;
                        }
                    };

                    let details =
                        crate::compaction::compaction_details_to_value(&result.details).ok();
                    (
                        result.summary,
                        result.first_kept_entry_id,
                        result.tokens_before,
                        details,
                        false,
                    )
                };

            let (messages_for_agent, compaction_entry) = {
                let mut guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                let from_hook = if from_extension { Some(true) } else { None };
                let entry_id = guard.append_compaction(
                    summary,
                    first_kept_entry_id,
                    tokens_before,
                    details,
                    from_hook,
                );
                let _ = guard.save().await;
                let compaction_entry = guard.get_entry(&entry_id).and_then(|entry| {
                    if let crate::session::SessionEntry::Compaction(compaction) = entry {
                        Some(compaction.clone())
                    } else {
                        None
                    }
                });
                (guard.to_messages_for_current_path(), compaction_entry)
            };

            {
                let mut agent_guard = match OwnedMutexGuard::lock(Arc::clone(&agent), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                agent_guard.replace_messages(messages_for_agent);
            }

            let (messages, usage) = {
                let guard = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
                conversation_from_session(&guard)
            };

            is_compacting.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::ConversationReset {
                    messages,
                    usage,
                    status: Some("Compaction complete".to_string()),
                },
            )
            .await;

            if let Some(manager) = extensions
                && let Some(compaction_entry) = compaction_entry
            {
                let _ = manager
                    .dispatch_event(
                        crate::extensions::ExtensionEventName::SessionCompact,
                        Some(json!({
                            "compactionEntry": compaction_entry,
                            "fromExtension": from_extension,
                        })),
                    )
                    .await;
            }
        });
        None
    }
}

// ---------------------------------------------------------------------------
// MessageRenderCache — per-message rendered content memoization (PERF-1)
// ---------------------------------------------------------------------------

use crate::interactive::state::MessageRole;
use std::cell::RefCell;

/// Lightweight cache key for a rendered conversation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MessageCacheKey {
    content_hash: u64,
    collapsed: bool,
    role: MessageRole,
}

/// Per-message render cache that stores the rendered output for each
/// `ConversationMessage`. Avoids re-rendering unchanged messages every frame.
///
/// Uses interior mutability (`RefCell`) because `view(&self)` cannot take
/// `&mut self` — same pattern as `FrameTimingStats`.
pub struct MessageRenderCache {
    /// Cached entries indexed by message position. `None` = cache miss.
    entries: RefCell<Vec<Option<(MessageCacheKey, String)>>>,
    /// Bumped on global invalidation: terminal resize, theme change,
    /// toggle-thinking, tool-expand toggle. All entries from a previous
    /// generation are considered stale.
    generation: std::cell::Cell<u64>,
    /// The generation at which each entry was cached. Stored separately
    /// to avoid duplicating generation in every entry.
    entry_generations: RefCell<Vec<u64>>,

    // -- PERF-2: Conversation prefix cache --
    // During streaming, only the tail (current_response/current_thinking)
    // changes. The prefix (all finalized messages) is cached here so
    // `build_conversation_content()` can skip re-iterating messages.
    /// Cached rendered content of all finalized messages.
    prefix: RefCell<String>,
    /// Number of messages included when the prefix was built.
    prefix_message_count: std::cell::Cell<usize>,
    /// The render-cache generation at which the prefix was built.
    /// If the generation has advanced, the prefix is stale.
    prefix_generation: std::cell::Cell<u64>,
}

impl MessageRenderCache {
    #[allow(clippy::missing_const_for_fn)]
    pub(super) fn new() -> Self {
        Self {
            entries: RefCell::new(Vec::new()),
            generation: std::cell::Cell::new(0),
            entry_generations: RefCell::new(Vec::new()),
            prefix: RefCell::new(String::new()),
            prefix_message_count: std::cell::Cell::new(0),
            prefix_generation: std::cell::Cell::new(0),
        }
    }

    /// Bump the generation counter, causing all cached entries and the
    /// conversation prefix to be considered stale on next lookup.
    /// O(1) — does not touch entries or the prefix buffer.
    pub(super) fn invalidate_all(&self) {
        self.generation.set(self.generation.get() + 1);
        // Prefix staleness is detected by comparing prefix_generation
        // with the current generation — no explicit flag needed.
    }

    /// Clear all cached entries and the prefix. Used on `/clear` or
    /// conversation reset.
    pub(super) fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.entry_generations.borrow_mut().clear();
        self.prefix.borrow_mut().clear();
        self.prefix_message_count.set(0);
        self.prefix_generation.set(0);
    }

    /// Look up the cached rendered string for message at `index`.
    /// Returns `Some(&str)` on cache hit, `None` on miss.
    #[cfg(test)]
    pub(super) fn get(&self, index: usize, key: &MessageCacheKey) -> Option<String> {
        let entries = self.entries.borrow();
        let gens = self.entry_generations.borrow();
        if index >= entries.len() {
            return None;
        }
        let generation = self.generation.get();
        if gens[index] != generation {
            return None;
        }
        entries[index].as_ref().and_then(|(cached_key, rendered)| {
            if cached_key == key {
                Some(rendered.clone())
            } else {
                None
            }
        })
    }

    /// Append cached content for message at `index` directly into `output`.
    ///
    /// Returns `true` on cache hit, `false` on miss. This avoids allocating
    /// a cloned `String` on every hit in the view hot path.
    pub(super) fn append_cached(
        &self,
        output: &mut String,
        index: usize,
        key: &MessageCacheKey,
    ) -> bool {
        let entries = self.entries.borrow();
        let gens = self.entry_generations.borrow();
        if index >= entries.len() || index >= gens.len() {
            return false;
        }
        if gens[index] != self.generation.get() {
            return false;
        }
        if let Some((cached_key, rendered)) = &entries[index]
            && cached_key == key
        {
            output.push_str(rendered);
            return true;
        }
        false
    }

    /// Store a rendered string for message at `index`.
    pub(super) fn put(&self, index: usize, key: MessageCacheKey, rendered: String) {
        let mut entries = self.entries.borrow_mut();
        let mut gens = self.entry_generations.borrow_mut();
        // Grow vectors if needed.
        if index >= entries.len() {
            entries.resize_with(index + 1, || None);
            gens.resize(index + 1, 0);
        }
        let generation = self.generation.get();
        entries[index] = Some((key, rendered));
        gens[index] = generation;
    }

    /// Compute the cache key for a conversation message.
    pub(super) fn compute_key(
        msg: &super::ConversationMessage,
        thinking_visible: bool,
        tools_expanded: bool,
    ) -> MessageCacheKey {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        msg.content.hash(&mut hasher);
        if thinking_visible && let Some(thinking) = &msg.thinking {
            thinking.hash(&mut hasher);
        }
        // Include tools_expanded in hash for tool messages since it affects rendering
        if msg.role == MessageRole::Tool {
            tools_expanded.hash(&mut hasher);
        }
        MessageCacheKey {
            content_hash: hasher.finish(),
            collapsed: msg.collapsed,
            role: msg.role,
        }
    }

    // -- PERF-2: Prefix cache accessors --

    /// Returns `true` if the cached prefix is still valid for the given
    /// message count. The prefix is stale when:
    /// - The message count changed (messages added/removed)
    /// - The render-cache generation advanced (theme/resize/toggle)
    /// - The prefix is empty and there are messages to render
    pub(super) const fn prefix_valid(&self, message_count: usize) -> bool {
        message_count > 0
            && self.prefix_message_count.get() == message_count
            && self.prefix_generation.get() == self.generation.get()
    }

    /// Return a clone of the cached prefix string.
    pub(super) fn prefix_get(&self) -> String {
        self.prefix.borrow().clone()
    }

    /// Append the cached prefix directly into `output`, avoiding a clone.
    ///
    /// PERF-7: This is the zero-copy alternative to `prefix_get()` for the
    /// streaming hot path where we build into a reusable buffer.
    pub(super) fn prefix_append_to(&self, output: &mut String) {
        output.push_str(&self.prefix.borrow());
    }

    /// Store a new prefix and snapshot the current message count / generation.
    pub(super) fn prefix_set(&self, content: &str, message_count: usize) {
        let mut p = self.prefix.borrow_mut();
        p.clear();
        p.push_str(content);
        self.prefix_message_count.set(message_count);
        self.prefix_generation.set(self.generation.get());
    }
}

// ---------------------------------------------------------------------------
// RenderBuffers — pre-allocated reusable buffers for view() hot path (PERF-7)
// ---------------------------------------------------------------------------

/// Pre-allocated buffers that are cleared and reused each frame, avoiding
/// repeated heap allocations in the 60fps render loop.
///
/// Uses `RefCell` for interior mutability because `view(&self)` cannot take
/// `&mut self` (same pattern as `FrameTimingStats` and `MessageRenderCache`).
pub struct RenderBuffers {
    /// Reusable buffer for `build_conversation_content()`.
    /// Taken via `std::mem::take`, built into, then returned.
    /// The buffer is put back (capacity preserved) after use.
    conversation: RefCell<String>,
    /// Reusable header buffer retained for render-buffer tests.
    #[cfg(test)]
    header: RefCell<String>,
    /// Reusable footer buffer retained for render-buffer tests.
    #[cfg(test)]
    footer: RefCell<String>,
    /// Capacity of the previous frame's final view output.
    /// Used to pre-allocate the next frame's output String via
    /// `String::with_capacity()`, avoiding incremental grows.
    view_capacity_hint: std::cell::Cell<usize>,
}

/// Default initial capacity for the view assembly buffer.
/// 80 columns x 24 rows x 4 bytes (UTF-8 + ANSI escapes).
const INITIAL_VIEW_CAPACITY: usize = 80 * 24 * 4;

/// Initial capacity for header/footer buffers (small: ~512 bytes typical).
#[cfg(test)]
const INITIAL_CHROME_CAPACITY: usize = 512;

impl RenderBuffers {
    pub(super) fn new() -> Self {
        Self {
            conversation: RefCell::new(String::with_capacity(INITIAL_VIEW_CAPACITY)),
            #[cfg(test)]
            header: RefCell::new(String::with_capacity(INITIAL_CHROME_CAPACITY)),
            #[cfg(test)]
            footer: RefCell::new(String::with_capacity(INITIAL_CHROME_CAPACITY)),
            view_capacity_hint: std::cell::Cell::new(INITIAL_VIEW_CAPACITY),
        }
    }

    /// Take the conversation buffer for reuse. The caller must put it back
    /// via [`return_conversation_buffer`] after building content.
    pub(super) fn take_conversation_buffer(&self) -> String {
        let mut buf = self.conversation.borrow_mut();
        let mut taken = std::mem::take(&mut *buf);
        taken.clear();
        taken
    }

    /// Return the conversation buffer after use, preserving its heap capacity.
    pub(super) fn return_conversation_buffer(&self, buf: String) {
        *self.conversation.borrow_mut() = buf;
    }

    /// Borrow the header buffer mutably, clearing it for reuse.
    /// The caller writes into the returned `RefMut` via `push_str` / `write!`.
    #[cfg(test)]
    pub(super) fn header_buf(&self) -> std::cell::RefMut<'_, String> {
        let mut buf = self.header.borrow_mut();
        buf.clear();
        buf
    }

    /// Borrow the footer buffer mutably, clearing it for reuse.
    #[cfg(test)]
    pub(super) fn footer_buf(&self) -> std::cell::RefMut<'_, String> {
        let mut buf = self.footer.borrow_mut();
        buf.clear();
        buf
    }

    /// Get the capacity hint for the next frame's view assembly.
    pub(super) const fn view_capacity_hint(&self) -> usize {
        self.view_capacity_hint.get()
    }

    /// Update the capacity hint after a frame completes.
    pub(super) fn set_view_capacity_hint(&self, capacity: usize) {
        self.view_capacity_hint.set(capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::state::{ConversationMessage, MessageRole};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ========================================================================
    // FrameTimingStats unit tests (PERF-3)
    // ========================================================================

    fn make_stats(enabled: bool) -> FrameTimingStats {
        FrameTimingStats {
            frame_times_us: std::cell::RefCell::new(VecDeque::new()),
            content_build_times_us: std::cell::RefCell::new(VecDeque::new()),
            viewport_sync_times_us: std::cell::RefCell::new(VecDeque::new()),
            update_times_us: VecDeque::new(),
            total_frames: std::cell::Cell::new(0),
            budget_exceeded_count: std::cell::Cell::new(0),
            enabled,
        }
    }

    #[test]
    fn frame_timing_disabled_by_default() {
        let stats = make_stats(false);
        stats.record_frame(5000);
        assert_eq!(stats.total_frames.get(), 0);
        assert!(stats.frame_times_us.borrow().is_empty());
    }

    #[test]
    fn frame_timing_records_when_enabled() {
        let stats = make_stats(true);
        stats.record_frame(5000);
        stats.record_frame(10_000);
        stats.record_frame(20_000);
        assert_eq!(stats.total_frames.get(), 3);
        assert_eq!(stats.budget_exceeded_count.get(), 1);
        assert_eq!(stats.frame_times_us.borrow().len(), 3);
    }

    #[test]
    fn frame_timing_content_build_records() {
        let stats = make_stats(true);
        stats.record_content_build(1500);
        stats.record_content_build(2500);
        assert_eq!(stats.content_build_times_us.borrow().len(), 2);
    }

    #[test]
    fn frame_timing_viewport_sync_records() {
        let stats = make_stats(true);
        stats.record_viewport_sync(800);
        stats.record_viewport_sync(1200);
        assert_eq!(stats.viewport_sync_times_us.borrow().len(), 2);
    }

    #[test]
    fn frame_timing_update_records() {
        let mut stats = make_stats(true);
        stats.record_update(500);
        stats.record_update(1000);
        assert_eq!(stats.update_times_us.len(), 2);
    }

    #[test]
    fn frame_timing_rolling_window_evicts_oldest() {
        let stats = make_stats(true);
        for i in 0..=FRAME_TIMING_WINDOW as u64 {
            stats.record_frame(i * 100);
        }
        assert_eq!(stats.frame_times_us.borrow().len(), FRAME_TIMING_WINDOW);
        assert_eq!(*stats.frame_times_us.borrow().front().unwrap(), 100);
    }

    #[test]
    fn frame_timing_percentiles_empty() {
        let empty = VecDeque::new();
        assert_eq!(FrameTimingStats::percentiles(&empty), (0, 0, 0, 0));
    }

    #[test]
    fn frame_timing_percentiles_single_value() {
        let mut times = VecDeque::new();
        times.push_back(5000);
        assert_eq!(
            FrameTimingStats::percentiles(&times),
            (5000, 5000, 5000, 5000)
        );
    }

    #[test]
    fn frame_timing_percentiles_known_distribution() {
        let mut times = VecDeque::new();
        for i in 1..=100 {
            times.push_back(i * 1000);
        }
        let (p50, p95, p99, p999) = FrameTimingStats::percentiles(&times);
        assert_eq!(p50, 51_000);
        assert_eq!(p95, 96_000);
        assert_eq!(p99, 100_000);
        assert_eq!(p999, 100_000);
    }

    #[test]
    fn frame_timing_summary_disabled() {
        let stats = make_stats(false);
        assert!(stats.summary().contains("disabled"));
    }

    #[test]
    fn frame_timing_summary_enabled_contains_stats() {
        let stats = make_stats(true);
        stats.record_frame(5000);
        stats.record_content_build(2000);
        let summary = stats.summary();
        assert!(summary.contains("Frame timing"));
        assert!(summary.contains("view()"));
        assert!(summary.contains("content"));
        assert!(summary.contains("viewport"));
        assert!(summary.contains("update()"));
        assert!(summary.contains("p999"));
        assert!(summary.contains("Budget exceeded"));
    }

    #[test]
    fn frame_timing_snapshot_is_structured_and_redaction_safe() {
        let mut stats = make_stats(false);
        stats.enable_for_test();
        stats.record_frame(8_000);
        stats.record_frame(FRAME_BUDGET_US + 25);
        stats.record_content_build(3_000);
        stats.record_viewport_sync(1_000);
        stats.record_update(750);

        let fixture = json!({
            "message_count": 600,
            "tool_preview_count": 12,
            "model_count": 80,
            "branch_count": 64,
        });
        let snapshot = stats.snapshot_json("large_conversation", &fixture);

        assert_eq!(snapshot["schema"], "pi.tui.frame_budget.v1");
        assert_eq!(snapshot["surface"], "large_conversation");
        assert_eq!(snapshot["enabled"], true);
        assert_eq!(snapshot["samples"]["frame"]["count"], 2);
        assert_eq!(snapshot["samples"]["frame"]["over_budget_count"], 1);
        assert_eq!(snapshot["verdict"], "warn");
        assert_eq!(snapshot["redaction"]["prompt_content"], "omitted");
        assert_eq!(snapshot["redaction"]["tool_payload_content"], "omitted");
        let serialized = snapshot.to_string();
        assert!(
            !serialized.contains("payload content"),
            "snapshot must not include prompt or tool payload text"
        );
    }

    #[test]
    fn frame_timing_budget_exceeded_counts_correctly() {
        let stats = make_stats(true);
        stats.record_frame(10_000);
        stats.record_frame(16_000);
        stats.record_frame(FRAME_BUDGET_US);
        assert_eq!(stats.budget_exceeded_count.get(), 0);
        stats.record_frame(FRAME_BUDGET_US + 1);
        stats.record_frame(20_000);
        assert_eq!(stats.budget_exceeded_count.get(), 2);
    }

    #[test]
    fn frame_timing_exposes_current_p99() {
        let stats = make_stats(true);
        stats.record_frame(8_000);
        stats.record_frame(12_000);
        stats.record_frame(40_000);
        assert_eq!(stats.frame_p99_us(), 40_000);
    }

    #[test]
    fn tui_pressure_controller_uses_latency_output_and_pending_events() {
        assert_eq!(
            TuiPressureController::decide(0, 1024, 0).level,
            TuiPressureLevel::Normal
        );

        let by_frame =
            TuiPressureController::decide(TuiPressureController::ELEVATED_FRAME_P99_US, 1024, 0);
        assert_eq!(by_frame.level, TuiPressureLevel::Elevated);
        assert!(by_frame.throttle_tool_updates);

        let by_output =
            TuiPressureController::decide(0, TuiPressureController::ELEVATED_TOOL_OUTPUT_BYTES, 0);
        assert_eq!(by_output.level, TuiPressureLevel::Elevated);

        let by_pending =
            TuiPressureController::decide(0, 1024, TuiPressureController::HIGH_PENDING_TOOL_EVENTS);
        assert_eq!(by_pending.level, TuiPressureLevel::High);
        assert!(by_pending.flush_interval > by_frame.flush_interval);
    }

    // ========================================================================
    // MemoryMonitor unit tests (PERF-6)
    // ========================================================================

    struct MockRssReader {
        value: Arc<AtomicUsize>,
    }

    impl MockRssReader {
        fn new(initial: usize) -> (Self, Arc<AtomicUsize>) {
            let shared = Arc::new(AtomicUsize::new(initial));
            (
                Self {
                    value: Arc::clone(&shared),
                },
                shared,
            )
        }
    }

    impl RssReader for MockRssReader {
        fn read_rss_bytes(&self) -> Option<usize> {
            Some(self.value.load(Ordering::Relaxed))
        }
    }

    fn make_memory_monitor(initial_rss: usize) -> (MemoryMonitor, Arc<AtomicUsize>) {
        let (reader, shared) = MockRssReader::new(initial_rss);
        let mut monitor = MemoryMonitor::new(Box::new(reader));
        monitor.sample_interval = std::time::Duration::ZERO;
        (monitor, shared)
    }

    #[test]
    fn memory_level_classification() {
        assert_eq!(MemoryLevel::from_rss_bytes(0), MemoryLevel::Normal);
        assert_eq!(MemoryLevel::from_rss_bytes(30_000_000), MemoryLevel::Normal);
        assert_eq!(MemoryLevel::from_rss_bytes(49_999_999), MemoryLevel::Normal);
        assert_eq!(
            MemoryLevel::from_rss_bytes(50_000_000),
            MemoryLevel::Warning
        );
        assert_eq!(
            MemoryLevel::from_rss_bytes(99_999_999),
            MemoryLevel::Warning
        );
        assert_eq!(
            MemoryLevel::from_rss_bytes(100_000_000),
            MemoryLevel::Pressure
        );
        assert_eq!(
            MemoryLevel::from_rss_bytes(199_999_999),
            MemoryLevel::Pressure
        );
        assert_eq!(
            MemoryLevel::from_rss_bytes(200_000_000),
            MemoryLevel::Critical
        );
        assert_eq!(
            MemoryLevel::from_rss_bytes(500_000_000),
            MemoryLevel::Critical
        );
    }

    #[test]
    fn memory_monitor_sampling_tracks_rss_and_peak() {
        let (mut monitor, shared) = make_memory_monitor(30_000_000);
        monitor.maybe_sample();
        assert_eq!(monitor.current_rss_bytes, 30_000_000);
        assert_eq!(monitor.peak_rss_bytes, 30_000_000);
        assert_eq!(monitor.level, MemoryLevel::Normal);

        shared.store(60_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert_eq!(monitor.current_rss_bytes, 60_000_000);
        assert_eq!(monitor.peak_rss_bytes, 60_000_000);
        assert_eq!(monitor.level, MemoryLevel::Warning);

        shared.store(20_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert_eq!(monitor.current_rss_bytes, 20_000_000);
        assert_eq!(monitor.peak_rss_bytes, 60_000_000);
        assert_eq!(monitor.level, MemoryLevel::Normal);
    }

    #[test]
    fn memory_monitor_pressure_starts_collapsing() {
        let (mut monitor, shared) = make_memory_monitor(10_000_000);
        monitor.maybe_sample();
        assert!(!monitor.collapsing);

        shared.store(120_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert_eq!(monitor.level, MemoryLevel::Pressure);
        assert!(monitor.collapsing);
    }

    #[test]
    fn memory_monitor_hysteresis_stops_collapsing() {
        let (mut monitor, shared) = make_memory_monitor(120_000_000);
        monitor.maybe_sample();
        assert!(monitor.collapsing);

        // 70MB < 80MB relief threshold => collapsing stops.
        // Level is Warning (50-100MB), not Normal.
        shared.store(70_000_000, Ordering::Relaxed);
        monitor.resample_now();
        assert!(!monitor.collapsing);
        assert_eq!(monitor.level, MemoryLevel::Warning);

        // Drop fully below 50MB => Normal.
        shared.store(30_000_000, Ordering::Relaxed);
        monitor.resample_now();
        assert!(!monitor.collapsing);
        assert_eq!(monitor.level, MemoryLevel::Normal);
    }

    #[test]
    fn memory_monitor_summary_format() {
        let (mut monitor, _) = make_memory_monitor(55_000_000);
        monitor.maybe_sample();
        let summary = monitor.summary();
        assert!(summary.contains("55.0MB"));
        assert!(summary.contains("Warning"));
    }

    #[test]
    fn memory_monitor_should_force_degraded_only_at_critical() {
        let (mut monitor, shared) = make_memory_monitor(10_000_000);
        monitor.maybe_sample();
        assert!(!monitor.should_force_degraded());

        shared.store(60_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert!(!monitor.should_force_degraded());

        shared.store(150_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert!(!monitor.should_force_degraded());

        shared.store(250_000_000, Ordering::Relaxed);
        monitor.maybe_sample();
        assert!(monitor.should_force_degraded());
    }

    #[test]
    fn memory_progressive_collapse_ordering() {
        let messages = [
            ConversationMessage::new(MessageRole::User, "hello".into(), None),
            ConversationMessage::new(MessageRole::Tool, "output 1".into(), None),
            ConversationMessage::new(MessageRole::Assistant, "response".into(), None),
            ConversationMessage::new(MessageRole::Tool, "output 2".into(), None),
            ConversationMessage::new(MessageRole::Tool, "output 3".into(), None),
        ];
        let mut next_idx = 0usize;
        let mut found = Vec::new();
        loop {
            let result = messages[next_idx..]
                .iter()
                .enumerate()
                .find(|(_, m)| matches!(m.role, MessageRole::Tool) && !m.collapsed)
                .map(|(i, _)| next_idx + i);
            match result {
                Some(idx) => {
                    found.push(idx);
                    next_idx = idx + 1;
                }
                None => break,
            }
        }
        assert_eq!(found, vec![1, 3, 4]);
    }

    #[test]
    fn memory_critical_truncation_keeps_last_messages() {
        let mut messages: Vec<ConversationMessage> = (0..50)
            .map(|i| {
                ConversationMessage::new(
                    if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    format!("msg {i}"),
                    None,
                )
            })
            .collect();

        let msg_count = messages.len();
        assert!(msg_count > CRITICAL_KEEP_MESSAGES);
        let remove_count = msg_count - CRITICAL_KEEP_MESSAGES;
        messages.drain(..remove_count);
        messages.insert(
            0,
            ConversationMessage::new(MessageRole::System, "[truncated]".into(), None),
        );

        assert_eq!(messages[0].role, MessageRole::System);
        assert!(messages[0].content.contains("truncated"));
        assert_eq!(messages.len(), CRITICAL_KEEP_MESSAGES + 1);
        assert_eq!(messages.last().unwrap().content, "msg 49");
    }

    // ========================================================================
    // Cross-platform fallback tests (PERF-CROSS-PLATFORM / bd-32sj0)
    // Verify MemoryMonitor degrades gracefully when RssReader returns None
    // (i.e. on non-Linux platforms where /proc/self/statm is unavailable).
    // ========================================================================

    /// An RssReader that always returns None — simulates non-Linux platforms.
    struct NullRssReader;

    impl RssReader for NullRssReader {
        fn read_rss_bytes(&self) -> Option<usize> {
            None
        }
    }

    fn make_null_memory_monitor() -> MemoryMonitor {
        let mut monitor = MemoryMonitor::new(Box::new(NullRssReader));
        monitor.sample_interval = std::time::Duration::ZERO;
        monitor
    }

    #[test]
    fn memory_monitor_null_reader_stays_normal() {
        let mut monitor = make_null_memory_monitor();
        // maybe_sample should return false (no level change) when reader returns None.
        assert!(!monitor.maybe_sample());
        assert_eq!(monitor.level, MemoryLevel::Normal);
        assert_eq!(monitor.current_rss_bytes, 0);
        assert_eq!(monitor.peak_rss_bytes, 0);
        assert!(!monitor.collapsing);
        assert!(!monitor.should_force_degraded());
    }

    #[test]
    fn memory_monitor_null_reader_repeated_sampling_stable() {
        let mut monitor = make_null_memory_monitor();
        // Many sampling cycles should not cause drift, panic, or state corruption.
        for _ in 0..100 {
            assert!(!monitor.maybe_sample());
        }
        assert_eq!(monitor.level, MemoryLevel::Normal);
        assert_eq!(monitor.current_rss_bytes, 0);
        assert_eq!(monitor.peak_rss_bytes, 0);
    }

    #[test]
    fn memory_monitor_null_reader_resample_now_no_panic() {
        let mut monitor = make_null_memory_monitor();
        // resample_now should silently do nothing when reader returns None.
        monitor.resample_now();
        assert_eq!(monitor.level, MemoryLevel::Normal);
        assert_eq!(monitor.current_rss_bytes, 0);
    }

    #[test]
    fn memory_monitor_null_reader_summary_shows_zero() {
        let mut monitor = make_null_memory_monitor();
        monitor.maybe_sample();
        let summary = monitor.summary();
        assert!(
            summary.contains("0.0MB"),
            "Summary should show 0.0MB when no RSS available, got: {summary}"
        );
        assert!(
            summary.contains("Normal"),
            "Summary should show Normal level when no RSS available, got: {summary}"
        );
    }

    #[test]
    fn frame_timing_operates_independently_of_memory_pressure() {
        // FrameTimingStats does not depend on MemoryMonitor or CPU pressure.
        // It should work correctly even when memory monitoring is unavailable.
        let stats = make_stats(true);
        // Simulate a realistic frame sequence.
        stats.record_frame(8_000);
        stats.record_frame(12_000);
        stats.record_frame(FRAME_BUDGET_US + 500);
        stats.record_content_build(3_000);
        stats.record_viewport_sync(1_500);
        // Verify all counters updated correctly.
        assert_eq!(stats.total_frames.get(), 3);
        assert_eq!(stats.budget_exceeded_count.get(), 1);
        assert_eq!(stats.content_build_times_us.borrow().len(), 1);
        assert_eq!(stats.viewport_sync_times_us.borrow().len(), 1);
        // Summary should produce valid output without any memory/CPU context.
        let summary = stats.summary();
        assert!(
            summary.contains("Frame timing"),
            "Summary should work without memory pressure context"
        );
        assert!(
            summary.contains("Budget exceeded: 1"),
            "Budget exceeded count should be accurate"
        );
    }

    #[test]
    fn proc_self_rss_reader_returns_some_on_linux() {
        let reader = ProcSelfRssReader;
        let result = reader.read_rss_bytes();
        #[cfg(target_os = "linux")]
        assert!(result.is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(result.is_none());
    }

    // --- MessageRenderCache tests (PERF-1) ---

    #[test]
    fn cache_hit_returns_same_content() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);
        cache.put(0, key.clone(), "rendered-hello".to_string());
        assert_eq!(cache.get(0, &key), Some("rendered-hello".to_string()));
    }

    #[test]
    fn append_cached_writes_output_on_hit() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);
        cache.put(0, key.clone(), "rendered-hello".to_string());

        let mut output = String::new();
        assert!(cache.append_cached(&mut output, 0, &key));
        assert_eq!(output, "rendered-hello");
    }

    #[test]
    fn append_cached_noop_on_miss() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);

        let mut output = String::new();
        assert!(!cache.append_cached(&mut output, 0, &key));
        assert!(output.is_empty());
    }

    #[test]
    fn cache_miss_after_content_change() {
        let cache = MessageRenderCache::new();
        let msg1 = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key1 = MessageRenderCache::compute_key(&msg1, false, true);
        cache.put(0, key1, "rendered-hello".to_string());

        let msg2 = ConversationMessage::new(MessageRole::User, "Goodbye".to_string(), None);
        let key2 = MessageRenderCache::compute_key(&msg2, false, true);
        assert_eq!(cache.get(0, &key2), None);
    }

    #[test]
    fn tool_message_cache_miss_when_collapse_toggles() {
        let cache = MessageRenderCache::new();
        let mut msg = ConversationMessage::tool("Tool bash:\nline1\nline2".to_string());
        let key_expanded = MessageRenderCache::compute_key(&msg, false, true);
        cache.put(0, key_expanded.clone(), "expanded-output".to_string());

        // Toggle collapse
        msg.collapsed = !msg.collapsed;
        let key_collapsed = MessageRenderCache::compute_key(&msg, false, true);
        assert_ne!(key_expanded, key_collapsed);
        assert_eq!(cache.get(0, &key_collapsed), None);
    }

    #[test]
    fn generation_bump_forces_full_miss() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::Assistant, "Response".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);
        cache.put(0, key.clone(), "old-render".to_string());

        // Simulate terminal resize → generation bump
        cache.invalidate_all();
        assert_eq!(cache.get(0, &key), None);
    }

    #[test]
    fn clear_removes_all_entries() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);
        cache.put(0, key.clone(), "rendered".to_string());
        cache.put(1, key.clone(), "rendered2".to_string());
        cache.clear();
        assert_eq!(cache.get(0, &key), None);
        assert_eq!(cache.get(1, &key), None);
    }

    #[test]
    fn thinking_visibility_changes_key() {
        let msg = ConversationMessage::new(
            MessageRole::Assistant,
            "Response".to_string(),
            Some("Thinking...".to_string()),
        );
        let key_visible = MessageRenderCache::compute_key(&msg, true, true);
        let key_hidden = MessageRenderCache::compute_key(&msg, false, true);
        assert_ne!(
            key_visible, key_hidden,
            "Thinking visibility should change the key"
        );
    }

    #[test]
    fn tools_expanded_changes_key_for_tool_messages() {
        let msg = ConversationMessage::tool("Tool output\nline1\nline2".to_string());
        let key_expanded = MessageRenderCache::compute_key(&msg, false, true);
        let key_collapsed = MessageRenderCache::compute_key(&msg, false, false);
        assert_ne!(
            key_expanded, key_collapsed,
            "tools_expanded should change key for tool messages"
        );
    }

    #[test]
    fn out_of_bounds_index_returns_none() {
        let cache = MessageRenderCache::new();
        let msg = ConversationMessage::new(MessageRole::User, "Hello".to_string(), None);
        let key = MessageRenderCache::compute_key(&msg, false, true);
        assert_eq!(cache.get(42, &key), None);
    }

    // --- Prefix cache tests (PERF-2) ---

    #[test]
    fn prefix_initially_invalid() {
        let cache = MessageRenderCache::new();
        assert!(!cache.prefix_valid(0));
        assert!(!cache.prefix_valid(1));
    }

    #[test]
    fn prefix_valid_after_set() {
        let cache = MessageRenderCache::new();
        cache.prefix_set("rendered-prefix", 5);
        assert!(cache.prefix_valid(5));
        assert_eq!(cache.prefix_get(), "rendered-prefix");
    }

    #[test]
    fn prefix_invalid_after_message_count_change() {
        let cache = MessageRenderCache::new();
        cache.prefix_set("prefix-for-5", 5);
        assert!(cache.prefix_valid(5));
        // New message added → count changed
        assert!(!cache.prefix_valid(6));
    }

    #[test]
    fn prefix_invalid_after_invalidate_all() {
        let cache = MessageRenderCache::new();
        cache.prefix_set("prefix", 3);
        assert!(cache.prefix_valid(3));
        // Simulate theme change / resize / toggle
        cache.invalidate_all();
        assert!(!cache.prefix_valid(3));
    }

    #[test]
    fn prefix_cleared_on_clear() {
        let cache = MessageRenderCache::new();
        cache.prefix_set("prefix", 3);
        cache.clear();
        assert!(!cache.prefix_valid(3));
        assert!(cache.prefix_get().is_empty());
    }

    #[test]
    fn prefix_revalidates_after_rebuild() {
        let cache = MessageRenderCache::new();
        cache.prefix_set("old-prefix", 3);
        cache.invalidate_all();
        assert!(!cache.prefix_valid(3));
        // Full rebuild sets new prefix
        cache.prefix_set("new-prefix", 3);
        assert!(cache.prefix_valid(3));
        assert_eq!(cache.prefix_get(), "new-prefix");
    }

    // ========================================================================
    // RenderBuffers unit tests (PERF-7)
    // ========================================================================

    #[test]
    fn render_buffers_initial_capacity_hint() {
        let rb = RenderBuffers::new();
        assert_eq!(rb.view_capacity_hint(), INITIAL_VIEW_CAPACITY);
    }

    #[test]
    fn render_buffers_capacity_hint_updates() {
        let rb = RenderBuffers::new();
        rb.set_view_capacity_hint(12_345);
        assert_eq!(rb.view_capacity_hint(), 12_345);
    }

    #[test]
    fn render_buffers_take_returns_cleared_buffer() {
        let rb = RenderBuffers::new();
        let buf = rb.take_conversation_buffer();
        assert!(buf.is_empty());
        assert!(buf.capacity() >= INITIAL_VIEW_CAPACITY);
    }

    #[test]
    fn render_buffers_return_preserves_capacity() {
        let rb = RenderBuffers::new();
        let mut buf = rb.take_conversation_buffer();
        // Write enough data to grow the buffer well beyond initial capacity.
        let big = "x".repeat(INITIAL_VIEW_CAPACITY * 3);
        buf.push_str(&big);
        let grown_cap = buf.capacity();
        rb.return_conversation_buffer(buf);

        // Taking again should reuse the grown allocation (cleared but same cap).
        let buf2 = rb.take_conversation_buffer();
        assert!(buf2.is_empty());
        assert_eq!(buf2.capacity(), grown_cap);
    }

    #[test]
    fn render_buffers_take_without_return_gives_fresh() {
        let rb = RenderBuffers::new();
        let buf1 = rb.take_conversation_buffer();
        // Don't return buf1 — simulates the buffer being consumed.
        drop(buf1);
        // Next take gets a fresh (empty, zero-cap) String.
        let buf2 = rb.take_conversation_buffer();
        assert!(buf2.is_empty());
    }

    #[test]
    fn render_buffers_header_buf_cleared_on_each_call() {
        let rb = RenderBuffers::new();
        {
            let mut hdr = rb.header_buf();
            hdr.push_str("old header");
        }
        // Next call should return a cleared buffer with preserved capacity.
        let hdr = rb.header_buf();
        assert!(hdr.is_empty());
        assert!(hdr.capacity() >= INITIAL_CHROME_CAPACITY);
    }

    #[test]
    fn render_buffers_footer_buf_cleared_on_each_call() {
        let rb = RenderBuffers::new();
        {
            let mut ftr = rb.footer_buf();
            ftr.push_str("old footer");
        }
        let ftr = rb.footer_buf();
        assert!(ftr.is_empty());
        assert!(ftr.capacity() >= INITIAL_CHROME_CAPACITY);
    }
}
