//! Conversation-owned browser namespaces over the registry's existing live scope.
//!
//! The outer lease covers selection, the complete native call and publication.
//! It never moves a browser between owners or evicts another owner's work.
//! Attached endpoints remain explicitly shared external browser profiles.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{BrowserTool, cdp, mock, output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::jobs::JobSessionScope;
use crate::tools::ToolOutput;

pub(super) mod batch;

const CAPACITY: usize = 4;
const MAX_OWNER_BYTES: usize = 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

pub(super) struct Namespace {
    pub(super) mock_state: Mutex<mock::State>,
    pub(super) live_state: Arc<asupersync::sync::Mutex<cdp::Session>>,
}

impl Default for Namespace {
    fn default() -> Self {
        Self {
            mock_state: Mutex::new(mock::State::default()),
            live_state: Arc::new(asupersync::sync::Mutex::new(cdp::Session::default())),
        }
    }
}

#[derive(Default)]
pub(super) struct Sessions {
    scope: JobSessionScope,
    execution: futures::lock::Mutex<()>,
    namespaces: Mutex<BTreeMap<String, Arc<Namespace>>>,
}

impl Sessions {
    pub(super) fn bind(&mut self, scope: JobSessionScope) {
        self.scope = scope;
        let retired = std::mem::take(
            self.namespaces
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // Rebinding transfers an owned tool to a new registry, not its browsers.
        // Process guards run without the namespace-map mutex held.
        drop(retired);
    }

    fn select(&self, id: &str, retain: bool) -> Result<(Arc<Namespace>, bool)> {
        let mut namespaces = self
            .namespaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(namespace) = namespaces.get(id) {
            return Ok((Arc::clone(namespace), true));
        }
        if retain && namespaces.len() >= CAPACITY {
            return Err(error(
                "BROWSER_SESSION_CAPACITY: four browser namespaces are retained; switch to an owning conversation and stop or release its browser before starting another",
            ));
        }
        let namespace = Arc::new(Namespace::default());
        if retain {
            namespaces.insert(id.to_owned(), Arc::clone(&namespace));
        }
        drop(namespaces);
        Ok((namespace, false))
    }

    fn release(&self, id: &str) -> bool {
        let retired = self
            .namespaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        let released = retired.is_some();
        // Keep the execution lease, but never the map lock, during cleanup.
        // Session field order stops owned processes before upload copies drop.
        drop(retired);
        released
    }

    async fn identity(&self, budget: &Budget) -> Result<String> {
        let id = budget.wait(self.scope.session_id()).await?.map_err(|_| {
            error("BROWSER_SESSION_UNAVAILABLE: current conversation identity is unavailable")
        })?;
        if id.len() > MAX_OWNER_BYTES {
            return Err(error(
                "BROWSER_SESSION_UNAVAILABLE: conversation identity exceeds 1024 bytes",
            ));
        }
        Ok(id)
    }

    async fn require_identity(&self, expected: &str, budget: &Budget) -> Result<()> {
        if self.identity(budget).await? != expected {
            return Err(error(
                "BROWSER_SESSION_CHANGED: conversation changed; request was not retargeted and any admitted effects remain with its original browser",
            ));
        }
        Ok(())
    }

    /// Keep the same native future across checks. During a contended identity
    /// lookup, suspend its polling rather than continuing to dispatch browser
    /// commands with unconfirmed ownership. A mismatch drops it permanently.
    /// Already-sent remote commands and page scripts are not rolled back.
    async fn run_owned<F: Future>(
        &self,
        expected: &str,
        budget: &Budget,
        future: F,
    ) -> Result<F::Output> {
        let mut operation = Box::pin(budget.owner.with_current(future));
        loop {
            self.require_identity(expected, budget).await?;
            budget.checkpoint()?;
            if let Poll::Ready(result) = futures::poll!(operation.as_mut()) {
                budget.checkpoint()?;
                return Ok(result);
            }
            if !budget.owner.capabilities().time {
                return Err(error(
                    "BROWSER_SESSION_PERMISSION: active ownership observation requires timer capability",
                ));
            }
            let wait = budget
                .deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let time = budget.owner.time();
            let sleeper = Box::pin(time.sleep(wait));
            match futures::future::select(operation, sleeper).await {
                futures::future::Either::Left((result, _)) => {
                    budget.checkpoint()?;
                    return Ok(result);
                }
                futures::future::Either::Right(((), pending)) => operation = pending,
            }
        }
    }

    pub(super) async fn execute(&self, tool: &BrowserTool, args: &Value) -> Result<ToolOutput> {
        let action = required(args, "action")?;
        let budget = Budget::new(args, action)?;
        let batch = batch::Plan::parse(args)?;
        if batch.is_some() && tool.is_mock() {
            return Err(error(
                "BROWSER_BATCH_NATIVE_REQUIRED: batches require the native browser backend",
            ));
        }
        if action == "release" {
            validate_release(args)?;
        }
        // Capture before queueing. A later resolver value cannot assign this
        // waiting invocation to another conversation, even after A -> B -> A
        // was observed at an admission boundary.
        let id = self.identity(&budget).await?;
        let _execution = budget.wait(self.execution.lock()).await?;
        self.require_identity(&id, &budget).await?;
        if action == "release" {
            let released = self.release(&id);
            self.require_identity(&id, &budget).await?;
            return Ok(output(
                "Released this conversation's browser state; owned-process cleanup requested. Externally attached browsers are not stopped.",
                json!({"action":"release", "released":released,
                    "browserSession":{"scope":"session", "retained":false, "capacity":CAPACITY}}),
            ));
        }

        // Empty lifecycle probes need no retained slot and work even at capacity.
        // Other attempts reserve before dispatch: errors/cancellation can occur
        // after a browser or upload has been adopted. Never destroy such state
        // by assuming an error means no side effect. Explicit release retires it.
        let retain = !matches!(action, "status" | "stop");
        let (namespace, reused) = self.select(&id, retain)?;
        let result = if let Some(batch) = batch {
            batch.execute(self, &id, &budget, tool, &namespace).await
        } else {
            self.run_owned(&id, &budget, tool.execute_in_namespace(&namespace, args))
                .await?
        };
        // Stop belongs to the captured owner even if the frontend switched
        // during its execution. Do not postpone bookkeeping past that check.
        let stopped = action == "stop" && result.as_ref().is_ok_and(|value| !value.is_error);
        if stopped {
            self.release(&id);
        }
        self.require_identity(&id, &budget).await?;
        let mut result = result?;
        if let Some(details) = result.details.as_mut().and_then(Value::as_object_mut) {
            details.insert(
                "browserSession".into(),
                json!({
                    "scope":"session", "reused":reused, "retained":!stopped && (retain || reused),
                    "capacity":CAPACITY,
                }),
            );
        }
        Ok(result)
    }
}

fn validate_release(args: &Value) -> Result<()> {
    if args.as_object().is_none_or(|object| {
        object
            .keys()
            .any(|key| !matches!(key.as_str(), "action" | "timeout_ms"))
    }) {
        return Err(error(
            "BROWSER_SESSION_INVALID: release accepts only action and timeout_ms; its owner is selected by the host",
        ));
    }
    Ok(())
}

/// One budget includes owner lookup, queue wait, native execution and the final
/// ownership check. A ready result cannot hide cancellation or overrun. The
/// native backend keeps its own watchdog too; this outer budget never extends it.
struct Budget {
    owner: AgentCx,
    deadline: Instant,
}

impl Budget {
    fn new(args: &Value, action: &str) -> Result<Self> {
        let milliseconds = match args.get("timeout_ms") {
            None => {
                if action == "wait_for" {
                    5000
                } else {
                    30_000
                }
            }
            Some(value) => value
                .as_u64()
                .filter(|value| (1..=120_000).contains(value))
                .ok_or_else(|| error("timeout_ms must be an integer in 1..=120000"))?,
        };
        Ok(Self {
            owner: AgentCx::for_current_or_request(),
            deadline: Instant::now() + Duration::from_millis(milliseconds),
        })
    }

    fn checkpoint(&self) -> Result<()> {
        self.owner.checkpoint()
            .map_err(|_| error("BROWSER_SESSION_CANCELLED: browser operation cancelled; admitted effects are not rolled back"))?;
        if Instant::now() >= self.deadline {
            return Err(error(
                "BROWSER_SESSION_TIMEOUT: browser operation timed out; admitted effects are not rolled back",
            ));
        }
        Ok(())
    }

    async fn wait<F: Future>(&self, future: F) -> Result<F::Output> {
        let mut operation = Box::pin(self.owner.with_current(future));
        let result = loop {
            self.checkpoint()?;
            if let Poll::Ready(result) = futures::poll!(operation.as_mut()) {
                break result;
            }
            if !self.owner.capabilities().time {
                return Err(error(
                    "BROWSER_SESSION_PERMISSION: waiting for browser ownership requires timer capability",
                ));
            }
            let wait = self
                .deadline
                .saturating_duration_since(Instant::now())
                .min(POLL_INTERVAL);
            let time = self.owner.time();
            let sleeper = Box::pin(time.sleep(wait));
            match futures::future::select(operation, sleeper).await {
                futures::future::Either::Left((result, _)) => break result,
                futures::future::Either::Right(((), pending)) => operation = pending,
            }
        };
        self.checkpoint()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
