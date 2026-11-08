//! Bounded, nonblocking handoff between the native UI and browser transports.
//!
//! Only sanitized snapshots cross the output boundary. Input keeps its socket
//! identity and control generation until the UI makes its own admission check.

use super::{ControlMode, WebFrame, WebFrameType, WebRemoteManager, WebRemoteSettings};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_FRAME_WIDTH: u16 = 1000;
pub const MAX_FRAME_HEIGHT: u16 = 500;
pub const MAX_PASTE_BYTES: usize = 16 * 1024;
pub const INPUT_QUEUE_CAPACITY: usize = 64;
const MAX_SECRET_BYTES: usize = 1024 * 1024;
const MAX_SECRETS: usize = 4096;
const MAX_EXTRA_PATTERNS: usize = 256;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebRemoteInput {
    Key {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
    },
    Paste {
        text: String,
    },
    Abort,
}

impl std::fmt::Debug for WebRemoteInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key { .. } => formatter.write_str("WebRemoteInput::Key(<redacted>)"),
            Self::Paste { text } => formatter
                .debug_struct("WebRemoteInput::Paste")
                .field("bytes", &text.len())
                .finish(),
            Self::Abort => formatter.write_str("WebRemoteInput::Abort"),
        }
    }
}

impl WebRemoteInput {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Key { key, .. } => {
                let named = matches!(
                    key.as_str(),
                    "Enter"
                        | "Backspace"
                        | "Delete"
                        | "Tab"
                        | "Escape"
                        | "ArrowUp"
                        | "ArrowDown"
                        | "ArrowLeft"
                        | "ArrowRight"
                        | "Home"
                        | "End"
                        | "PageUp"
                        | "PageDown"
                );
                let mut chars = key.chars();
                let character =
                    chars.next().is_some_and(|ch| !ch.is_control()) && chars.next().is_none();
                if !named && !character {
                    return Err("unsupported remote key".to_string());
                }
            }
            Self::Paste { text } => {
                if text.len() > MAX_PASTE_BYTES
                    || text
                        .chars()
                        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
                {
                    return Err("remote paste exceeds its size or character limit".to_string());
                }
            }
            Self::Abort => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum WebRemoteEvent {
    TakeoverRequested {
        client_id: String,
    },
    Input {
        client_id: String,
        epoch: u64,
        input: WebRemoteInput,
    },
    Refresh,
}

#[derive(Default)]
struct Frames {
    keyframe: Option<Arc<WebFrame>>,
    patch: Option<(u64, Arc<WebFrame>)>,
}

#[derive(Default)]
struct Redactor {
    supplied: Vec<String>,
    remembered: Vec<String>,
    remembered_patterns: Vec<regex::Regex>,
    remembered_block: bool,
    history_unavailable: bool,
    cached_protection: Option<(ProtectionKey, Arc<RedactionProtection>)>,
    unavailable: bool,
    privacy: Option<Arc<Mutex<crate::compaction::CompactionPrivacy>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtectionFailure {
    Budget,
    Unavailable,
}

#[derive(PartialEq, Eq)]
struct ProtectionKey {
    values: Vec<String>,
    patterns: Vec<String>,
    block: bool,
    failure: Option<ProtectionFailure>,
}

impl ProtectionKey {
    fn failed(failure: ProtectionFailure) -> Self {
        Self {
            values: Vec::new(),
            patterns: Vec::new(),
            block: false,
            failure: Some(failure),
        }
    }
}

struct RedactionProtection {
    known: Option<regex::Regex>,
    compact_known: Option<regex::Regex>,
    extra: Vec<regex::Regex>,
    block: bool,
    failure: Option<ProtectionFailure>,
}

/// Immutable credential protection for one native render. Classify complete
/// source text before wrapping or clipping, then publish with this same scope.
/// Cloning shares compiled detectors; neither values nor patterns are exposed.
#[derive(Clone)]
pub struct WebRemoteRedactionScope {
    revision: u64,
    protection: Arc<RedactionProtection>,
}

impl std::fmt::Debug for WebRemoteRedactionScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebRemoteRedactionScope")
            .field("revision", &self.revision)
            .field("unavailable", &self.protection.failure.is_some())
            .finish_non_exhaustive()
    }
}

impl WebRemoteRedactionScope {
    /// Cache source classification by both its own revision and this revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether complete source text must be withheld before viewport clipping.
    /// Oversized text or unavailable protection always fails closed.
    #[must_use]
    pub fn is_sensitive(&self, text: &str) -> bool {
        if text.len() > MAX_FRAME_BYTES || self.protection.failure.is_some() {
            return true;
        }
        if contains_sensitive(text, self.protection.known.as_ref(), &self.protection.extra) {
            return true;
        }
        let compact = text
            .chars()
            .filter(|ch| !is_frame_padding(*ch))
            .collect::<String>();
        contains_sensitive(
            &compact,
            self.protection.compact_known.as_ref(),
            &self.protection.extra,
        )
    }

    fn redact(&self, text: &str) -> String {
        let failure = if text.len() > MAX_FRAME_BYTES {
            Some(ProtectionFailure::Budget)
        } else {
            self.protection.failure
        };
        match failure {
            Some(ProtectionFailure::Budget) => {
                "Remote frame withheld: credential protection exceeds the scan budget.".to_string()
            }
            Some(ProtectionFailure::Unavailable) => {
                "Remote frame withheld: credential protection is unavailable.".to_string()
            }
            None => redact_text(
                text,
                self.protection.known.as_ref(),
                self.protection.compact_known.as_ref(),
                &self.protection.extra,
                self.protection.block,
            ),
        }
    }

    fn same_protection(&self, other: &Self) -> bool {
        self.revision == other.revision && Arc::ptr_eq(&self.protection, &other.protection)
    }
}

fn contains_sensitive(text: &str, known: Option<&regex::Regex>, extra: &[regex::Regex]) -> bool {
    known.is_some_and(|known| known.is_match(text)) || crate::secrets::contains_secret(text, extra)
}

struct BridgeState {
    manager: WebRemoteManager,
    frames: Mutex<Frames>,
    redactor: Mutex<Redactor>,
    input_tx: mpsc::SyncSender<WebRemoteEvent>,
    input_rx: Mutex<mpsc::Receiver<WebRemoteEvent>>,
    closed: AtomicBool,
    privacy_revision: AtomicU64,
}

#[derive(Clone)]
pub struct WebRemoteBridge {
    state: Arc<BridgeState>,
}

impl std::fmt::Debug for WebRemoteBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebRemoteBridge")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl WebRemoteBridge {
    #[must_use]
    pub fn new(settings: WebRemoteSettings) -> Self {
        let (input_tx, input_rx) = mpsc::sync_channel(INPUT_QUEUE_CAPACITY);
        Self {
            state: Arc::new(BridgeState {
                manager: WebRemoteManager::new(settings),
                frames: Mutex::new(Frames::default()),
                redactor: Mutex::new(Redactor::default()),
                input_tx,
                input_rx: Mutex::new(input_rx),
                closed: AtomicBool::new(false),
                privacy_revision: AtomicU64::new(0),
            }),
        }
    }

    #[must_use]
    pub fn manager(&self) -> &WebRemoteManager {
        &self.state.manager
    }

    /// Register current session credentials in addition to live pairing tokens.
    /// A protection set beyond the bounded scan budget withholds the frame.
    pub fn set_secrets(&self, mut secrets: Vec<String>) {
        secrets.retain(|value| !value.is_empty());
        secrets.sort_unstable();
        secrets.dedup();
        let mut redactor = self
            .state
            .redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unavailable = secrets.len() > MAX_SECRETS
            || secrets
                .iter()
                .try_fold(0usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|bytes| bytes > MAX_SECRET_BYTES);
        redactor.unavailable = unavailable;
        redactor.history_unavailable |= unavailable;
        let previous = std::mem::take(&mut redactor.supplied);
        redactor.remember(previous, Vec::new(), false);
        redactor.supplied = if redactor.unavailable {
            Vec::new()
        } else {
            secrets
        };
        self.invalidate_frames();
    }

    /// Protect locally entered credentials for the remainder of this listener's
    /// life, including later echoes after an authentication dialog has closed.
    pub fn remember_secrets(&self, secrets: Vec<String>) {
        self.state
            .redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remember(secrets, Vec::new(), false);
        self.invalidate_frames();
    }

    /// Keep newly learned credentials protected while the agent is streaming.
    pub fn set_privacy_state(&self, privacy: Arc<Mutex<crate::compaction::CompactionPrivacy>>) {
        let mut redactor = self
            .state
            .redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = redactor.privacy.take() {
            match previous.lock() {
                Ok(previous) => {
                    let (values, patterns, block) = previous.remote_frame_protection();
                    redactor.remember(values, patterns, block);
                }
                Err(_) => redactor.history_unavailable = true,
            }
        }
        redactor.privacy = Some(privacy);
        self.invalidate_frames();
    }

    fn invalidate_frames(&self) {
        let mut frames = self
            .state
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state.privacy_revision.fetch_add(1, Ordering::AcqRel);
        *frames = Frames::default();
    }

    /// Capture immutable detectors for complete-source classification. Live
    /// shared vault changes are observed here even when their Arc is unchanged.
    #[must_use]
    pub fn redaction_scope(&self) -> WebRemoteRedactionScope {
        let mut redactor = self
            .state
            .redactor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (protection, changed) = redactor.protection(self.manager());
        if changed {
            // A previously safe cached source or frame may now contain a newly
            // learned credential. Invalidate it under the same publication lock.
            self.invalidate_frames();
        }
        WebRemoteRedactionScope {
            revision: self.state.privacy_revision.load(Ordering::Acquire),
            protection,
        }
    }

    /// Publish at most one current full frame and one immediate-generation diff.
    /// No renderer operation waits for a browser, socket, or outbound queue.
    pub fn publish_text(&self, width: u16, height: u16, data: String) -> bool {
        self.publish_text_inner(None, width, height, data)
    }

    /// Publish a frame classified before clipping with this exact scope.
    /// A changed live vault retires the stale render instead of attempting to
    /// recognize a partial credential in already-clipped terminal cells.
    pub fn publish_text_with_scope(
        &self,
        scope: &WebRemoteRedactionScope,
        width: u16,
        height: u16,
        data: String,
    ) -> bool {
        self.publish_text_inner(Some(scope), width, height, data)
    }

    fn publish_text_inner(
        &self,
        expected_scope: Option<&WebRemoteRedactionScope>,
        width: u16,
        height: u16,
        data: String,
    ) -> bool {
        if self.is_closed()
            || width == 0
            || height == 0
            || width > MAX_FRAME_WIDTH
            || height > MAX_FRAME_HEIGHT
            || data.len() > MAX_FRAME_BYTES
        {
            return false;
        }
        let scope = self.redaction_scope();
        if expected_scope.is_some_and(|expected| !expected.same_protection(&scope)) {
            return false;
        }
        let data = scope.redact(&data);
        let mut frames = self
            .state
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_closed()
            || self.state.privacy_revision.load(Ordering::Acquire) != scope.revision()
        {
            return false;
        }
        if frames.keyframe.as_ref().is_some_and(|previous| {
            previous.width == width && previous.height == height && previous.data == data
        }) {
            return false;
        }
        let current =
            Arc::new(
                self.manager()
                    .next_frame(WebFrameType::Keyframe, width, height, data),
            );
        frames.patch = frames.keyframe.as_ref().and_then(|previous| {
            if previous.width != width || previous.height != height {
                return None;
            }
            let old_lines = previous.data.split('\n').collect::<Vec<_>>();
            let new_lines = current.data.split('\n').collect::<Vec<_>>();
            if old_lines.len() != new_lines.len() {
                return None;
            }
            let lines = new_lines
                .iter()
                .zip(&old_lines)
                .enumerate()
                .filter_map(|(row, (new, old))| (new != old).then_some((row, *new)))
                .collect::<Vec<_>>();
            let patch_data =
                serde_json::json!({"base_seq": previous.seq, "lines": lines}).to_string();
            // A full frame is smaller for short screens or widespread changes.
            if patch_data.len() >= current.data.len() {
                return None;
            }
            Some((
                previous.seq,
                Arc::new(WebFrame {
                    frame_type: WebFrameType::Patch,
                    data: patch_data,
                    ..(*current).clone()
                }),
            ))
        });
        frames.keyframe = Some(current);
        true
    }

    /// A slow viewer skips history and receives the latest complete keyframe.
    #[must_use]
    pub fn frame_after(&self, sequence: u64) -> Option<Arc<WebFrame>> {
        let frames = self
            .state
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = frames.keyframe.as_ref()?;
        if current.seq == sequence {
            return None;
        }
        if let Some((base, patch)) = &frames.patch
            && *base == sequence
        {
            return Some(Arc::clone(patch));
        }
        Some(Arc::clone(current))
    }

    pub fn queue_takeover(&self, client_id: &str) -> Result<ControlMode, String> {
        let mode = self.manager().request_takeover(client_id)?;
        if mode == ControlMode::TakeoverPendingApproval
            && let Err(error) = self.enqueue(WebRemoteEvent::TakeoverRequested {
                client_id: client_id.to_string(),
            })
        {
            self.manager().deny_takeover(client_id);
            return Err(error);
        }
        Ok(mode)
    }

    pub fn queue_input(
        &self,
        client_id: &str,
        sequence: u64,
        input: WebRemoteInput,
    ) -> Result<(), String> {
        input.validate()?;
        let epoch = self.manager().authorize_input(client_id, sequence)?;
        self.enqueue(WebRemoteEvent::Input {
            client_id: client_id.to_string(),
            epoch,
            input,
        })
    }

    pub fn release_control(&self, client_id: &str) {
        self.manager().release_control(client_id);
        self.request_refresh();
    }

    pub fn request_refresh(&self) {
        let _ = self.enqueue(WebRemoteEvent::Refresh);
    }

    fn enqueue(&self, event: WebRemoteEvent) -> Result<(), String> {
        if self.is_closed() {
            return Err("web remote session is closed".to_string());
        }
        self.state.input_tx.try_send(event).map_err(|_| {
            "web remote input is busy; wait for the native interface to catch up".to_string()
        })
    }

    pub fn recv_event_timeout(&self, timeout: Duration) -> Option<WebRemoteEvent> {
        if self.is_closed() {
            return None;
        }
        self.state
            .input_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(timeout.min(Duration::from_millis(100)))
            .ok()
    }

    pub fn try_recv_event(&self) -> Option<WebRemoteEvent> {
        if self.is_closed() {
            return None;
        }
        self.state
            .input_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_recv()
            .ok()
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
        self.manager().revoke_control();
        for token in self.manager().tokens_for_redaction() {
            self.manager().revoke_token(&token);
        }
        for client in self.manager().clients() {
            self.manager().disconnect_client(&client.client_id);
        }
        *self
            .state
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Frames::default();
        let receiver = self
            .state
            .input_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while receiver.try_recv().is_ok() {}
    }
}

impl Redactor {
    fn protection(&mut self, manager: &WebRemoteManager) -> (Arc<RedactionProtection>, bool) {
        let (key, extra) = self.protection_key(manager);
        if let Some((previous, protection)) = &self.cached_protection
            && *previous == key
        {
            return (Arc::clone(protection), false);
        }
        let protection = Arc::new(self.compile_protection(&key, extra));
        self.cached_protection = Some((key, Arc::clone(&protection)));
        (protection, true)
    }

    fn protection_key(&mut self, manager: &WebRemoteManager) -> (ProtectionKey, Vec<regex::Regex>) {
        if self.unavailable || self.history_unavailable {
            return (ProtectionKey::failed(ProtectionFailure::Budget), Vec::new());
        }
        if let Some(privacy) = self.privacy.clone() {
            let Ok(privacy) = privacy.lock() else {
                return (
                    ProtectionKey::failed(ProtectionFailure::Unavailable),
                    Vec::new(),
                );
            };
            let (known, patterns, should_block) = privacy.remote_frame_protection();
            // Agent session resets can refresh the same shared mutex in place.
            // Retain every observed generation, not just pointer replacements.
            self.remember(known, patterns, should_block);
        }
        if self.history_unavailable {
            return (ProtectionKey::failed(ProtectionFailure::Budget), Vec::new());
        }
        let mut values = self.supplied.clone();
        values.extend(self.remembered.iter().cloned());
        let mut extra_patterns = self.remembered_patterns.clone();
        values.extend(manager.tokens_for_redaction());
        values.retain(|value| !value.is_empty());
        values.sort_unstable();
        values.dedup();
        extra_patterns.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        extra_patterns.dedup_by(|left, right| left.as_str() == right.as_str());
        if values.len() > MAX_SECRETS
            || values
                .iter()
                .try_fold(0usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|bytes| bytes > MAX_SECRET_BYTES)
            || extra_patterns.len() > MAX_EXTRA_PATTERNS
            || extra_patterns
                .iter()
                .try_fold(0usize, |total, pattern| {
                    total.checked_add(pattern.as_str().len())
                })
                .is_none_or(|bytes| bytes > MAX_SECRET_BYTES)
        {
            return (ProtectionKey::failed(ProtectionFailure::Budget), Vec::new());
        }
        let key = ProtectionKey {
            values,
            patterns: extra_patterns
                .iter()
                .map(|pattern| pattern.as_str().to_owned())
                .collect(),
            block: self.remembered_block,
            failure: None,
        };
        (key, extra_patterns)
    }

    fn compile_protection(
        &self,
        key: &ProtectionKey,
        extra: Vec<regex::Regex>,
    ) -> RedactionProtection {
        if let Some(failure) = key.failure {
            return RedactionProtection {
                known: None,
                compact_known: None,
                extra: Vec::new(),
                block: key.block,
                failure: Some(failure),
            };
        }
        // Changing only a custom pattern or block policy does not require
        // recompiling the literal detectors shared by previous snapshots.
        let cached_literals = self
            .cached_protection
            .as_ref()
            .filter(|(previous, protection)| {
                previous.values == key.values && protection.failure.is_none()
            });
        let (known, compact_known, failure) = if let Some((_, protection)) = cached_literals {
            (
                protection.known.clone(),
                protection.compact_known.clone(),
                None,
            )
        } else {
            let compact_values = key
                .values
                .iter()
                .map(|value| {
                    value
                        .chars()
                        .filter(|ch| !is_frame_padding(*ch))
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            match (
                compile_known_pattern(&key.values),
                compile_known_pattern(&compact_values),
            ) {
                (Ok(known), Ok(compact_known)) => (known, compact_known, None),
                _ => (None, None, Some(ProtectionFailure::Unavailable)),
            }
        };
        RedactionProtection {
            known,
            compact_known,
            extra,
            block: key.block,
            failure,
        }
    }

    fn remember(&mut self, values: Vec<String>, patterns: Vec<regex::Regex>, block: bool) {
        if self.history_unavailable {
            return;
        }
        self.remembered
            .extend(values.into_iter().filter(|value| !value.is_empty()));
        self.remembered.sort_unstable();
        self.remembered.dedup();
        self.remembered_patterns.extend(patterns);
        self.remembered_patterns
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        self.remembered_patterns
            .dedup_by(|left, right| left.as_str() == right.as_str());
        self.remembered_block |= block;
        self.history_unavailable = self.remembered.len() > MAX_SECRETS
            || self
                .remembered
                .iter()
                .try_fold(0usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|bytes| bytes > MAX_SECRET_BYTES)
            || self.remembered_patterns.len() > MAX_EXTRA_PATTERNS
            || self
                .remembered_patterns
                .iter()
                .try_fold(0usize, |total, pattern| {
                    total.checked_add(pattern.as_str().len())
                })
                .is_none_or(|bytes| bytes > MAX_SECRET_BYTES);
        if self.history_unavailable {
            self.remembered.clear();
            self.remembered_patterns.clear();
        }
    }
}

/// Scan the complete snapshot before diffing. A second projection removes
/// terminal wrapping/padding so a credential split across rows is still masked.
fn redact_text(
    text: &str,
    known: Option<&regex::Regex>,
    compact_known: Option<&regex::Regex>,
    extra: &[regex::Regex],
    block: bool,
) -> String {
    let mut spans = crate::secrets::scan(text, extra)
        .into_iter()
        .map(|hit| (hit.start, hit.end))
        .collect::<Vec<_>>();
    if let Some(known) = known {
        spans.extend(overlapping_known_spans(text, known));
    }
    let mut compact = String::with_capacity(text.len());
    let mut original = Vec::<u32>::with_capacity(text.len());
    for (offset, ch) in text.char_indices() {
        if is_frame_padding(ch) {
            continue;
        }
        compact.push(ch);
        original.extend(std::iter::repeat_n(
            u32::try_from(offset).unwrap_or(u32::MAX),
            ch.len_utf8(),
        ));
    }
    let mut compact_spans = crate::secrets::scan(&compact, extra)
        .into_iter()
        .map(|hit| (hit.start, hit.end))
        .collect::<Vec<_>>();
    if let Some(known) = compact_known {
        compact_spans.extend(overlapping_known_spans(&compact, known));
    }
    for (start, end) in compact_spans {
        if let (Some(&start), Some(&last)) = (
            original.get(start),
            end.checked_sub(1).and_then(|end| original.get(end)),
        ) {
            let last = last as usize;
            let end = last + text[last..].chars().next().map_or(0, char::len_utf8);
            spans.push((start as usize, end));
        }
    }
    if spans.is_empty() {
        return text.to_string();
    }
    if block {
        return "Remote frame withheld by the session's credential privacy policy.".to_string();
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut output = String::with_capacity(text.len());
    let mut span = 0;
    for (offset, ch) in text.char_indices() {
        while span < merged.len() && offset >= merged[span].1 {
            span += 1;
        }
        let masked = span < merged.len() && offset >= merged[span].0;
        output.push(if masked && !matches!(ch, '\n' | '\r') {
            '*'
        } else {
            ch
        });
    }
    output
}

fn is_frame_padding(ch: char) -> bool {
    ch.is_whitespace() || ('\u{2500}'..='\u{257f}').contains(&ch)
}

fn compile_known_pattern(values: &[String]) -> Result<Option<regex::Regex>, regex::Error> {
    // Prefer the longest literal at each position. The overlapping scanner
    // also covers matches that start inside another credential.
    let mut alternatives = values
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    alternatives
        .sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    alternatives.dedup();
    if alternatives.is_empty() {
        return Ok(None);
    }
    let pattern = alternatives
        .into_iter()
        .map(|value| regex::escape(value))
        .collect::<Vec<_>>()
        .join("|");
    regex::RegexBuilder::new(&pattern)
        .size_limit(4 * MAX_SECRET_BYTES)
        .build()
        .map(Some)
}

fn overlapping_known_spans(text: &str, known: &regex::Regex) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0;
    while let Some(hit) = known.find_at(text, offset) {
        if let Some(previous) = spans.last_mut()
            && hit.start() <= previous.1
        {
            previous.1 = previous.1.max(hit.end());
        } else {
            spans.push((hit.start(), hit.end()));
        }
        if hit.end() == text.len() {
            break;
        }
        offset = hit.start() + text[hit.start()..].chars().next().map_or(1, char::len_utf8);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_remote::TokenKind;

    #[test]
    fn publication_deduplicates_and_resynchronizes_lagging_viewers() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let first = format!(
            "{}\n{}\n{}",
            "a".repeat(100),
            "b".repeat(100),
            "c".repeat(100)
        );
        assert!(bridge.publish_text(100, 3, first.clone()));
        let initial = bridge.frame_after(0).unwrap();
        assert!(!bridge.publish_text(100, 3, first));
        assert!(bridge.frame_after(initial.seq).is_none());
        assert!(bridge.publish_text(
            100,
            3,
            format!("{}\nchanged\n{}", "a".repeat(100), "c".repeat(100))
        ));
        let patch = bridge.frame_after(initial.seq).unwrap();
        assert_eq!(patch.frame_type, WebFrameType::Patch);
        let body: serde_json::Value = serde_json::from_str(&patch.data).unwrap();
        assert_eq!(body["base_seq"], initial.seq);
        assert_eq!(body["lines"], serde_json::json!([[1, "changed"]]));
        assert_eq!(
            bridge.frame_after(0).unwrap().frame_type,
            WebFrameType::Keyframe
        );
        bridge.publish_text(100, 3, "latest".to_string());
        let recovered = bridge.frame_after(initial.seq).unwrap();
        assert_eq!(recovered.frame_type, WebFrameType::Keyframe);
        assert_eq!(recovered.data, "latest");
    }

    #[test]
    fn raw_secrets_and_wrapped_pairing_tokens_never_reach_frames_or_patches() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        bridge.set_secrets(vec![
            "custom-credential-canary".to_string(),
            "opaque private passphrase".to_string(),
        ]);
        bridge
            .manager()
            .issue_token("pairing-canary-private", TokenKind::View)
            .unwrap();
        bridge.publish_text(100, 5, "custom-credential-canary\npairing-canary-\n  │private\napi_key=sk-abcdefghijklmnop12345678\nopaque private\n  │passphrase".to_string());
        let frame = bridge.frame_after(0).unwrap();
        assert!(!frame.data.contains("custom-credential"));
        assert!(!frame.data.contains("pairing-canary"));
        assert!(!frame.data.contains("private"));
        assert!(!frame.data.contains("sk-abcdefghijklmnop"));
        assert!(!frame.data.contains("opaque"));
        assert!(!frame.data.contains("passphrase"));
        let previous = frame.seq;
        bridge.publish_text(
            100,
            5,
            "prefix\ncustom-credential-canary\npairing-canary-private\nunchanged".to_string(),
        );
        let wire = serde_json::to_string(&*bridge.frame_after(previous).unwrap()).unwrap();
        assert!(!wire.contains("custom-credential"));
        assert!(!wire.contains("pairing-canary"));
    }

    #[test]
    fn live_vault_updates_and_retired_session_credentials_remain_private() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let mut vault = crate::secrets::SecretVault::default();
        let old = "opaque-old-vault-value";
        let learned = "opaque-new-vault-value";
        crate::secrets::obfuscate(old, &mut vault, &[regex::Regex::new(old).unwrap()]);
        let settings = crate::secrets::SecretsSettings {
            extra_patterns: Some(vec!["CUSTOM_[0-9]+".to_string()]),
            ..Default::default()
        };
        let privacy = Arc::new(Mutex::new(
            crate::compaction::CompactionPrivacy::with_vault(Some(&settings), &vault),
        ));
        bridge.set_privacy_state(Arc::clone(&privacy));
        bridge.publish_text(100, 1, old.to_string());
        assert!(!bridge.frame_after(0).unwrap().data.contains(old));

        crate::secrets::obfuscate(learned, &mut vault, &[regex::Regex::new(learned).unwrap()]);
        *privacy.lock().unwrap() =
            crate::compaction::CompactionPrivacy::with_vault(Some(&settings), &vault);
        bridge.publish_text(100, 1, format!("{old} {learned}"));
        let live = bridge.frame_after(0).unwrap();
        assert!(!live.data.contains(old));
        assert!(!live.data.contains(learned));

        *privacy.lock().unwrap() = crate::compaction::CompactionPrivacy::default();
        bridge.publish_text(
            100,
            1,
            format!("same agent reset: {old} {learned} CUSTOM_987"),
        );
        let reset = bridge.frame_after(0).unwrap();
        assert!(!reset.data.contains(old));
        assert!(!reset.data.contains(learned));
        assert!(!reset.data.contains("CUSTOM_987"));

        bridge.set_privacy_state(Arc::new(Mutex::new(
            crate::compaction::CompactionPrivacy::default(),
        )));
        assert!(
            bridge.frame_after(0).is_none(),
            "old frame must be retired at a privacy switch"
        );
        bridge.publish_text(100, 1, format!("{old} {learned} CUSTOM_12345"));
        let retired = bridge.frame_after(0).unwrap();
        assert!(!retired.data.contains(old));
        assert!(!retired.data.contains(learned));
        assert!(!retired.data.contains("CUSTOM_12345"));
    }

    #[test]
    fn source_scope_classifies_a_secret_before_viewport_clipping() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let secret = "opaque-source-credential-987654";
        bridge.set_secrets(vec![secret.to_owned(), "private passphrase".to_owned()]);
        let scope = bridge.redaction_scope();
        let source = format!("Tool output before wrapping: {secret}\nordinary trailing text");
        let viewport_fragment = "credential-987654";

        assert!(scope.is_sensitive(&source));
        assert!(
            !scope.is_sensitive(viewport_fragment),
            "the clipped fragment alone cannot identify its credential"
        );
        assert!(scope.is_sensitive("private\n │passphrase"));
        assert!(scope.is_sensitive("api_key=sk-abcdefghijklmnop12345678"));
        assert!(!scope.is_sensitive("ordinary tool output"));
        assert!(scope.is_sensitive(&"x".repeat(MAX_FRAME_BYTES + 1)));
        assert!(!format!("{scope:?}").contains(secret));
        assert!(!format!("{scope:?}").contains("passphrase"));

        let repeated = bridge.redaction_scope();
        assert_eq!(scope.revision(), repeated.revision());
        assert!(scope.same_protection(&repeated));
        // The renderer withholds the complete classified source, rather than
        // attempting to redact an unrecognizable tail after layout clipped it.
        let rendered = if scope.is_sensitive(&source) {
            "[local interaction]"
        } else {
            viewport_fragment
        };
        assert!(bridge.publish_text_with_scope(&scope, 80, 1, rendered.to_owned()));
        assert_eq!(bridge.frame_after(0).unwrap().data, "[local interaction]");
    }

    #[test]
    fn live_vault_discovery_advances_scope_revision_and_rejects_a_stale_render() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let privacy = Arc::new(Mutex::new(crate::compaction::CompactionPrivacy::default()));
        bridge.set_privacy_state(Arc::clone(&privacy));
        let before = bridge.redaction_scope();
        let secret = "opaque-learned-scope-credential";
        assert!(!before.is_sensitive(secret));
        assert!(bridge.publish_text_with_scope(&before, 80, 1, "earlier frame".to_owned()));

        let mut vault = crate::secrets::SecretVault::default();
        crate::secrets::obfuscate(secret, &mut vault, &[regex::Regex::new(secret).unwrap()]);
        *privacy.lock().unwrap() = crate::compaction::CompactionPrivacy::with_vault(None, &vault);
        // No setter or explicit invalidation runs: publishing itself must
        // observe the shared vault change before comparing the render scope.
        assert!(!bridge.publish_text_with_scope(&before, 80, 1, "scope-credential".to_owned()));
        assert!(bridge.frame_after(0).is_none());
        let after = bridge.redaction_scope();
        assert!(after.revision() > before.revision());
        assert!(after.is_sensitive(secret));
        assert!(
            !before.is_sensitive(secret),
            "a captured scope stays immutable"
        );
        assert_eq!(after.revision(), bridge.redaction_scope().revision());
        assert!(bridge.publish_text_with_scope(&after, 80, 1, secret.to_owned()));
        assert!(!bridge.frame_after(0).unwrap().data.contains(secret));

        let other = WebRemoteBridge::new(WebRemoteSettings::default());
        assert!(!other.publish_text_with_scope(&after, 80, 1, "foreign scope".to_owned()));
    }

    #[test]
    fn scope_revisions_cover_custom_patterns_blocking_and_failed_protection() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let privacy = Arc::new(Mutex::new(crate::compaction::CompactionPrivacy::default()));
        bridge.set_privacy_state(Arc::clone(&privacy));
        let initial = bridge.redaction_scope();
        assert!(!initial.is_sensitive("INTERNAL_123"));

        let mut settings = crate::secrets::SecretsSettings {
            extra_patterns: Some(vec!["INTERNAL_[0-9]+".to_owned()]),
            ..Default::default()
        };
        *privacy.lock().unwrap() =
            crate::compaction::CompactionPrivacy::from_settings(Some(&settings));
        let patterned = bridge.redaction_scope();
        assert!(patterned.revision() > initial.revision());
        assert!(patterned.is_sensitive("INTERNAL_123"));
        assert_eq!(patterned.redact("INTERNAL_123"), "************");

        settings.mode = Some("block".to_owned());
        *privacy.lock().unwrap() =
            crate::compaction::CompactionPrivacy::from_settings(Some(&settings));
        let blocked = bridge.redaction_scope();
        assert!(blocked.revision() > patterned.revision());
        assert!(
            blocked
                .redact("INTERNAL_123")
                .starts_with("Remote frame withheld by")
        );
        assert_eq!(patterned.redact("INTERNAL_123"), "************");

        bridge.remember_secrets(vec!["x".repeat(MAX_SECRET_BYTES + 1)]);
        let failed = bridge.redaction_scope();
        assert!(failed.revision() > blocked.revision());
        assert!(failed.is_sensitive("ordinary text"));
        assert!(failed.is_sensitive(""));
        assert!(
            failed
                .redact("ordinary text")
                .starts_with("Remote frame withheld:")
        );
        assert_eq!(failed.revision(), bridge.redaction_scope().revision());
    }

    #[test]
    fn overlapping_credentials_are_fully_masked_and_protection_overflow_fails_closed() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        bridge.set_secrets(vec![
            "abc".to_string(),
            "abcdef".to_string(),
            "efgh".to_string(),
        ]);
        bridge.remember_secrets(vec!["local-login-value".to_string()]);
        bridge.publish_text(100, 1, "abcdefgh local-login-value".to_string());
        assert_eq!(
            bridge.frame_after(0).unwrap().data,
            "******** *****************"
        );

        bridge.remember_secrets(vec!["x".repeat(MAX_SECRET_BYTES + 1)]);
        bridge.set_secrets(Vec::new());
        bridge.publish_text(100, 1, "abcdefgh local-login-value".to_string());
        assert!(
            bridge
                .frame_after(0)
                .unwrap()
                .data
                .starts_with("Remote frame withheld:")
        );
    }

    #[test]
    fn input_requires_local_grant_and_old_queued_authority_dies_on_revoke() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        let manager = bridge.manager();
        manager
            .issue_token("steer-token", TokenKind::Steer)
            .unwrap();
        manager
            .connect_client("one", "127.0.0.1:1", Some("steer-token"))
            .unwrap();
        assert!(bridge.queue_input("one", 1, WebRemoteInput::Abort).is_err());
        assert_eq!(
            bridge.queue_takeover("one").unwrap(),
            ControlMode::TakeoverPendingApproval
        );
        assert!(matches!(
            bridge.try_recv_event(),
            Some(WebRemoteEvent::TakeoverRequested { .. })
        ));
        manager.approve_takeover("one").unwrap();
        bridge.queue_input("one", 1, WebRemoteInput::Abort).unwrap();
        let Some(WebRemoteEvent::Input {
            client_id, epoch, ..
        }) = bridge.try_recv_event()
        else {
            panic!("queued input missing");
        };
        assert!(manager.input_authorized(&client_id, epoch));
        manager.revoke_control();
        assert!(!manager.input_authorized(&client_id, epoch));
    }

    #[test]
    fn bounded_queue_and_shutdown_do_not_wait_for_consumers() {
        let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
        for _ in 0..INPUT_QUEUE_CAPACITY {
            bridge.enqueue(WebRemoteEvent::Refresh).unwrap();
        }
        assert!(bridge.enqueue(WebRemoteEvent::Refresh).is_err());
        bridge.publish_text(10, 1, "ready".to_string());
        bridge.close();
        assert!(bridge.is_closed());
        assert!(bridge.frame_after(0).is_none());
        assert!(bridge.try_recv_event().is_none());
        assert!(!bridge.publish_text(10, 1, "late".to_string()));
    }
}
