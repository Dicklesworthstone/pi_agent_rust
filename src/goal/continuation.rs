//! Opt-in goal continuation that stops when tool work stops changing.
//!
//! Observe native tool lifecycle events, never assistant claims of progress.
//! Fingerprints include the tool name, canonical arguments and visible result
//! blocks, but not call IDs, signatures or diagnostic details. A successful
//! operation already seen anywhere in THIS run is not new work. This catches
//! reordered parallel batches and multi-prompt cycles without keeping outputs.
//! This is a conservative activity heuristic, not verification of deliverables.

use super::{GoalControl, GoalDecision, GoalRun, GoalRunReport, GoalSession, GoalSnapshot, failure};
use crate::agent::AgentEvent;
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::model::ContentBlock;
use crate::sdk::AgentSessionHandle;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
mod tests;

const MAX_STEPS: u32 = 32;
const MAX_CALLS: usize = 256;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 16_384;
const MAX_DEPTH: usize = 32;
const MAX_BLOCKS: usize = 1024;
const MAX_ID_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 256;
type Fingerprint = [u8; 32];
type Admission<T = ()> = std::result::Result<T, ()>;

impl AgentSessionHandle {
    /// Execute at most `max_steps` native goal prompts, pausing for host review.
    ///
    /// Only new, successfully completed tool activity permits another prompt.
    /// Empty activity, repeated work (including cycles), or incomplete/budgeted
    /// observations pause instead. Text-only answers are valid results, but do
    /// not justify an automatic continuation. This method NEVER completes the
    /// objective: inspect the result and explicitly attest completion separately.
    /// Use `run_goal` or `run_goal_with_verifier` for a domain-specific policy.
    ///
    /// Each invocation gets a fresh activity history. Explicit resume remains
    /// necessary for a paused goal. Native prompt/token budgets, input recovery,
    /// persistence acknowledgements and the absolute deadline remain enforced
    /// by the shared driver. A prompt is not preempted by this activity check.
    ///
    /// `max_steps` must be 1..=32. Per prompt, observation is bounded to 256
    /// calls, 4 MiB of fingerprint input and 16,384 JSON nodes, depth <= 32.
    /// Going over a bound pauses after the native prompt; it never authorizes
    /// continuation using a partial observation or truncates tool output.
    pub fn run_goal_until_review<'a>(
        &'a mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        max_steps: u32,
        timeout: Duration,
        on_event: impl Fn(&GoalControl, AgentEvent) + Send + Sync + 'static,
    ) -> Result<GoalRun<impl Future<Output = GoalRunReport> + 'a>> {
        if !(1..=MAX_STEPS).contains(&max_steps) {
            return Err(failure("GOAL_STEPS", "goal run requires between 1 and 32 prompts"));
        }
        let tracker = Arc::new(Mutex::new(Tracker::new(max_steps)));
        let observed = Arc::clone(&tracker);
        self.run_goal(
            owner,
            expected,
            timeout,
            move |control, event| {
                {
                    let mut tracker = lock(&observed);
                    tracker.observe(&event);
                }
                // Do not call host code while holding our observation lock.
                on_event(control, event);
            },
            move |_, _| lock(&tracker).decide(),
        )
    }
}

impl GoalSession {
    /// Owned-session counterpart of `AgentSessionHandle::run_goal_until_review`.
    pub fn run_goal_until_review<'a>(
        &'a mut self,
        owner: &AgentCx,
        expected: &GoalSnapshot,
        max_steps: u32,
        timeout: Duration,
        on_event: impl Fn(&GoalControl, AgentEvent) + Send + Sync + 'static,
    ) -> Result<GoalRun<impl Future<Output = GoalRunReport> + 'a>> {
        self.session_mut()
            .run_goal_until_review(owner, expected, max_steps, timeout, on_event)
    }
}

fn lock(mutex: &Mutex<Tracker>) -> std::sync::MutexGuard<'_, Tracker> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            let mut guard = error.into_inner();
            guard.current.invalid = true;
            guard
        }
    }
}

struct Tracker {
    current: Observation,
    seen: BTreeSet<Fingerprint>,
    steps: u32,
    max_steps: u32,
    stopped: Option<&'static str>,
}

impl Tracker {
    fn new(max_steps: u32) -> Self {
        Self {
            current: Observation::default(),
            seen: BTreeSet::new(),
            steps: 0,
            max_steps,
            stopped: None,
        }
    }

    fn observe(&mut self, event: &AgentEvent) {
        if self.stopped.is_some() || self.current.invalid {
            return;
        }
        let result = match event {
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args } => {
                self.current.start(tool_call_id, tool_name, args)
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error } => {
                self.current.end(
                    tool_call_id,
                    tool_name,
                    &result.content,
                    *is_error || result.is_error,
                )
            }
            // Stream updates, assistant prose, retry messages and timestamps
            // are not evidence of new tool activity.
            _ => Ok(()),
        };
        if result.is_err() {
            self.current.invalid = true;
        }
    }

    fn decide(&mut self) -> GoalDecision {
        if let Some(reason) = self.stopped {
            return pause(reason);
        }
        self.steps = self.steps.saturating_add(1);
        let current = std::mem::take(&mut self.current);
        let reason = if self.steps >= self.max_steps {
            Some("[GOAL_STEP_LIMIT] Requested prompt limit reached; inspect the results before resuming or attesting completion.")
        } else if current.invalid || !current.pending.is_empty() {
            Some("[GOAL_PROGRESS_UNCERTAIN] Tool activity was incomplete or exceeded observation limits; review before continuing.")
        } else if current.completed.is_empty() {
            Some("[GOAL_NO_TOOL_PROGRESS] No successfully completed tool operation was observed; review the answer before continuing.")
        } else if current.completed.iter().all(|digest| self.seen.contains(digest)) {
            Some("[GOAL_REPEATED_TOOL_PROGRESS] This run has already observed all of these successful tool operations and results; review the repeated work before continuing.")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.stopped = Some(reason);
            return pause(reason);
        }
        // At most 31 completed observations of 256 fingerprints each. IDs,
        // arguments and outputs never enter the run-wide history.
        self.seen.extend(current.completed);
        GoalDecision::Continue {
            instruction: format!(
                "Continue the same objective with the next useful action and verification. \
                 Do not repeat unchanged tool work or claim completion on the user's behalf. \
                 This is prompt {} of at most {} in the user's bounded run.",
                self.steps.saturating_add(1),
                self.max_steps,
            ),
        }
    }
}

fn pause(reason: &str) -> GoalDecision {
    GoalDecision::Pause { reason: reason.to_string() }
}

struct Pending {
    name: String,
    digest: Sha256,
}

#[derive(Default)]
struct Observation {
    budget: Budget,
    ids: BTreeSet<String>,
    pending: BTreeMap<String, Pending>,
    completed: BTreeSet<Fingerprint>,
    invalid: bool,
}

impl Observation {
    fn start(&mut self, id: &str, name: &str, args: &Value) -> Admission {
        if id.is_empty() || id.len() > MAX_ID_BYTES || name.is_empty()
            || name.len() > MAX_NAME_BYTES || self.ids.len() >= MAX_CALLS
            || self.ids.contains(id)
        {
            return Err(());
        }
        self.budget.charge(id.len())?;
        let mut digest = Sha256::new();
        digest.update(b"pi.goal.tool-progress.v1\0");
        part(&mut digest, &mut self.budget, name.as_bytes())?;
        json(&mut digest, &mut self.budget, args, 0)?;
        self.ids.insert(id.to_string());
        self.pending.insert(id.to_string(), Pending { name: name.to_string(), digest });
        Ok(())
    }

    fn end(&mut self, id: &str, name: &str, blocks: &[ContentBlock], failed: bool) -> Admission {
        if id.len() > MAX_ID_BYTES || name.len() > MAX_NAME_BYTES {
            return Err(());
        }
        let Pending { name: expected, mut digest } = self.pending.remove(id).ok_or(())?;
        if expected != name {
            return Err(());
        }
        if failed {
            // Failed calls are accounted as observations, but cannot be the
            // sole reason to buy another prompt, even if their errors change.
            return Ok(());
        }
        if blocks.len() > MAX_BLOCKS {
            return Err(());
        }
        count(&mut digest, blocks.len())?;
        for block in blocks {
            content(&mut digest, &mut self.budget, block)?;
        }
        self.completed.insert(digest.finalize().into());
        Ok(())
    }
}

struct Budget {
    bytes: usize,
    nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self { bytes: MAX_BYTES, nodes: MAX_NODES }
    }
}

impl Budget {
    fn charge(&mut self, bytes: usize) -> Admission {
        self.bytes = self.bytes.checked_sub(bytes).ok_or(())?;
        Ok(())
    }

    fn node(&mut self) -> Admission {
        self.nodes = self.nodes.checked_sub(1).ok_or(())?;
        Ok(())
    }
}

fn count(digest: &mut Sha256, size: usize) -> Admission {
    digest.update(u64::try_from(size).map_err(|_| ())?.to_be_bytes());
    Ok(())
}

fn part(digest: &mut Sha256, budget: &mut Budget, bytes: &[u8]) -> Admission {
    budget.charge(bytes.len().checked_add(8).ok_or(())?)?;
    count(digest, bytes.len())?;
    digest.update(bytes);
    Ok(())
}

fn json(digest: &mut Sha256, budget: &mut Budget, value: &Value, depth: usize) -> Admission {
    if depth > MAX_DEPTH {
        return Err(());
    }
    budget.node()?;
    budget.charge(1)?;
    match value {
        Value::Null => digest.update([0]),
        Value::Bool(value) => digest.update([1, u8::from(*value)]),
        Value::Number(value) => {
            digest.update([2]);
            serde_json::to_writer(HashWriter { digest: &mut *digest, budget: &mut *budget }, value).map_err(|_| ())?;
            digest.update([255]);
        }
        Value::String(value) => {
            digest.update([3]);
            part(digest, budget, value.as_bytes())?;
        }
        Value::Array(values) => {
            if values.len() > budget.nodes { return Err(()); }
            digest.update([4]);
            count(digest, values.len())?;
            for value in values {
                json(digest, budget, value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            if values.len() > budget.nodes { return Err(()); }
            // Bound key collection and comparison work before sorting. Borrow
            // values; do not clone a potentially huge/deep JSON object.
            let key_bytes = values.keys().try_fold(0usize, |total, key| total.checked_add(key.len())).ok_or(())?;
            if key_bytes > budget.bytes { return Err(()); }
            let mut fields = values.iter().collect::<Vec<_>>();
            fields.sort_unstable_by(|left, right| left.0.cmp(right.0));
            digest.update([5]);
            count(digest, fields.len())?;
            for (key, value) in fields {
                budget.node()?;
                part(digest, budget, key.as_bytes())?;
                json(digest, budget, value, depth + 1)?;
            }
        }
    }
    Ok(())
}

struct HashWriter<'a> {
    digest: &'a mut Sha256,
    budget: &'a mut Budget,
}

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.budget.charge(bytes.len()).map_err(|()| std::io::Error::other("goal observation limit"))?;
        self.digest.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn content(digest: &mut Sha256, budget: &mut Budget, block: &ContentBlock) -> Admission {
    budget.node()?;
    budget.charge(1)?;
    match block {
        ContentBlock::Text(text) => {
            digest.update([0]);
            part(digest, budget, text.text.as_bytes())?;
        }
        ContentBlock::Image(image) => {
            digest.update([1]);
            part(digest, budget, image.mime_type.as_bytes())?;
            part(digest, budget, image.data.as_bytes())?;
        }
        ContentBlock::Media(media) => {
            digest.update([2]);
            part(digest, budget, media.mime_type.as_bytes())?;
            part(digest, budget, media.data.as_bytes())?;
            digest.update([u8::from(media.name.is_some())]);
            part(digest, budget, media.name.as_deref().unwrap_or_default().as_bytes())?;
        }
        ContentBlock::Thinking(thinking) => {
            digest.update([3]);
            part(digest, budget, thinking.thinking.as_bytes())?;
        }
        ContentBlock::RedactedThinking(thinking) => {
            digest.update([4]);
            part(digest, budget, thinking.data.as_bytes())?;
        }
        ContentBlock::ToolCall(call) => {
            digest.update([5]);
            part(digest, budget, call.name.as_bytes())?;
            json(digest, budget, &call.arguments, 0)?;
        }
    }
    Ok(())
}
