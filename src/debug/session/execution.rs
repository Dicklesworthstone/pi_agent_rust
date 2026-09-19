//! Per-thread suspension state. DAP stop/continue defaults are deliberately
//! asymmetric: absent allThreadsStopped means one thread, absent
//! allThreadsContinued means every thread. Never infer a stop for a running peer.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{ExecState, tool_err};
use crate::error::Result;

const MAX_THREADS: usize = 4096;

#[derive(Clone)]
struct Stop {
    revision: u64,
    reason: String,
    details: Value,
}

#[derive(Clone)]
enum Status {
    Running,
    Stopped(Arc<Stop>),
}

#[derive(Clone)]
struct Cell {
    status: Status,
    revision: u64,
}

#[derive(Clone)]
struct Thread {
    name: Option<String>,
    cell: Cell,
}

/// A resume owns only the transitions it made. Later events or requests have
/// different revisions and cannot be undone by an earlier response.
pub(super) struct Resume {
    baseline: Cell,
    threads: BTreeMap<u64, Thread>,
    selected: Option<u64>,
    thread: u64,
    revision: u64,
}

pub(super) struct Execution {
    baseline: Cell,
    threads: BTreeMap<u64, Thread>,
    selected: Option<u64>,
    revision: u64,
    exited: bool,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            baseline: Cell {
                status: Status::Running,
                revision: 0,
            },
            threads: BTreeMap::new(),
            selected: None,
            revision: 0,
            exited: false,
        }
    }
}

impl Execution {
    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

    /// Identity of this thread's current suspension, not a session-wide flag.
    pub(super) fn stamp(&self, id: u64) -> Option<u64> {
        self.stop(id).map(|stop| stop.revision)
    }

    fn advance(&mut self) -> Result<u64> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| tool_err("DAP_PROTOCOL", "execution revision exhausted"))?;
        Ok(self.revision)
    }

    fn insert(&mut self, id: u64) -> Result<&mut Thread> {
        if !self.threads.contains_key(&id) && self.threads.len() >= MAX_THREADS {
            return Err(tool_err(
                "DAP_THREAD_LIMIT",
                "debug session exceeds 4096 tracked threads",
            ));
        }
        Ok(self.threads.entry(id).or_insert_with(|| Thread {
            name: None,
            cell: self.baseline.clone(),
        }))
    }

    fn choose(&mut self) {
        if self.selected.is_some_and(|id| self.stop(id).is_some()) {
            return;
        }
        self.selected = self.threads.iter().find_map(|(id, thread)| {
            matches!(thread.cell.status, Status::Stopped(_)).then_some(*id)
        });
    }

    fn stop(&self, id: u64) -> Option<&Stop> {
        if self.exited {
            return None;
        }
        match &self.threads.get(&id)?.cell.status {
            Status::Stopped(stop) => Some(stop),
            Status::Running => None,
        }
    }

    pub(super) fn exit(&mut self) {
        self.exited = true;
        self.selected = None;
        self.baseline.status = Status::Running;
        self.threads.clear();
    }

    fn on_stopped(&mut self, body: &Value) -> Result<()> {
        let all = boolean(body, "allThreadsStopped", false)?;
        let id = thread_id(body.get("threadId"))?;
        if !all && id.is_none() {
            return Err(tool_err(
                "DAP_PROTOCOL",
                "single-thread stopped event omitted threadId",
            ));
        }
        let reason = body["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
            .ok_or_else(|| tool_err("DAP_PROTOCOL", "stopped event omitted reason"))?;
        let revision = self.advance()?;
        let reason: String = reason.chars().take(1024).collect();
        let mut details = json!({"reason":reason});
        for key in ["description", "text"] {
            if let Some(text) = body[key].as_str() {
                details[key] = json!(text.chars().take(4096).collect::<String>());
            }
        }
        if let Some(ids) = body["hitBreakpointIds"].as_array() {
            details["hitBreakpointIds"] = json!(
                ids.iter()
                    .filter_map(Value::as_i64)
                    .take(256)
                    .collect::<Vec<_>>()
            );
        }
        let cell = Cell {
            status: Status::Stopped(Arc::new(Stop {
                revision,
                reason,
                details,
            })),
            revision,
        };
        if all {
            self.baseline = cell.clone();
            for thread in self.threads.values_mut() {
                thread.cell = cell.clone();
            }
        }
        if let Some(id) = id {
            self.insert(id)?.cell = cell;
            if body["preserveFocusHint"] != true || self.selected.is_none() {
                self.selected = Some(id);
            }
        }
        self.choose();
        Ok(())
    }

    fn on_continued(&mut self, body: &Value) -> Result<()> {
        let all = boolean(body, "allThreadsContinued", true)?;
        let id = thread_id(body.get("threadId"))?;
        if !all && id.is_none() {
            return Err(tool_err(
                "DAP_PROTOCOL",
                "partial continued event omitted threadId",
            ));
        }
        let revision = self.advance()?;
        let cell = Cell {
            status: Status::Running,
            revision,
        };
        if all {
            self.baseline = cell.clone();
            for thread in self.threads.values_mut() {
                thread.cell = cell.clone();
            }
        }
        if let Some(id) = id {
            self.insert(id)?.cell = cell;
        }
        self.choose();
        Ok(())
    }

    pub(super) fn event(&mut self, name: &str, body: &Value) -> Result<()> {
        if self.exited {
            return Ok(());
        }
        match name {
            "stopped" => self.on_stopped(body)?,
            "continued" => self.on_continued(body)?,
            "thread" => {
                let id = thread_id(body.get("threadId"))?
                    .ok_or_else(|| tool_err("DAP_PROTOCOL", "thread event omitted threadId"))?;
                match body["reason"].as_str() {
                    Some("started") => {
                        let revision = self.advance()?;
                        self.insert(id)?.cell = Cell {
                            status: Status::Running,
                            revision,
                        };
                    }
                    Some("exited") => {
                        self.advance()?;
                        self.threads.remove(&id);
                    }
                    _ => return Err(tool_err("DAP_PROTOCOL", "invalid thread event reason")),
                }
                self.choose();
            }
            "terminated" | "exited" => {
                self.advance()?;
                self.exit();
            }
            _ => {}
        }
        Ok(())
    }

    /// A threads reply may race thread events. Only an unchanged revision can
    /// reconcile membership; otherwise return the reply without granting new
    /// inspection authority and let the next threads query refresh membership.
    pub(super) fn observe_threads(&mut self, body: &Value, revision: u64) -> Result<()> {
        let entries = body["threads"]
            .as_array()
            .filter(|entries| entries.len() <= MAX_THREADS)
            .ok_or_else(|| {
                tool_err(
                    "DAP_PROTOCOL",
                    "threads response must contain at most 4096 threads",
                )
            })?;
        let mut names = BTreeMap::new();
        for entry in entries {
            let id = thread_id(entry.get("id"))?
                .ok_or_else(|| tool_err("DAP_PROTOCOL", "thread inventory omitted id"))?;
            let name = entry["name"]
                .as_str()
                .ok_or_else(|| tool_err("DAP_PROTOCOL", "thread inventory omitted name"))?;
            if names
                .insert(id, name.chars().take(1024).collect::<String>())
                .is_some()
            {
                return Err(tool_err("DAP_PROTOCOL", "duplicate thread ID in inventory"));
            }
        }
        if self.exited || revision != self.revision {
            return Ok(());
        }
        self.threads.retain(|id, _| names.contains_key(id));
        for (id, name) in names {
            self.insert(id)?.name = Some(name);
        }
        self.choose();
        Ok(())
    }

    pub(super) fn require(&self, selected: Option<u64>) -> Result<u64> {
        if self.exited {
            return Err(tool_err(
                "DAP_STATE_EXITED",
                "debuggee or adapter has exited",
            ));
        }
        if let Some(id) = selected.or(self.selected) {
            match self.threads.get(&id).map(|thread| &thread.cell.status) {
                Some(Status::Stopped(_)) => return Ok(id),
                Some(Status::Running) => {
                    return Err(tool_err(
                        "DAP_STATE_RUNNING",
                        format!("thread {id} is running; pause it before inspection"),
                    ));
                }
                None => {
                    return Err(tool_err(
                        "DAP_THREAD_UNKNOWN",
                        format!("thread {id} is not in the current inventory; query threads first"),
                    ));
                }
            }
        }
        if self.threads.is_empty() && matches!(self.baseline.status, Status::Stopped(_)) {
            return Ok(0);
        }
        Err(tool_err("DAP_STATE_RUNNING", "no known thread is stopped"))
    }

    pub(super) fn aggregate(&self) -> ExecState {
        if self.exited {
            return ExecState::Exited;
        }
        if let Some(id) = self.selected
            && let Some(stop) = self.stop(id)
        {
            return ExecState::Stopped {
                thread_id: id,
                reason: stop.reason.clone(),
            };
        }
        if self.threads.is_empty()
            && let Status::Stopped(stop) = &self.baseline.status
        {
            return ExecState::Stopped {
                thread_id: 0,
                reason: stop.reason.clone(),
            };
        }
        ExecState::Running
    }

    pub(super) fn snapshot(&self) -> Value {
        let threads: Vec<_> = self
            .threads
            .iter()
            .map(|(id, thread)| {
                let mut value = json!({"id":id,"name":thread.name,"state":"running"});
                if let Status::Stopped(stop) = &thread.cell.status {
                    value["state"] = json!("stopped");
                    value["stopRevision"] = json!(stop.revision);
                    value["stop"] = stop.details.clone();
                }
                value
            })
            .collect();
        json!({
            "state":self.aggregate(), "selectedThreadId":self.selected,
            "revision":self.revision, "threads":threads,
            "allThreadsStopped":!self.exited && matches!(self.baseline.status, Status::Stopped(_))
                && self.threads.values().all(|thread| matches!(thread.cell.status, Status::Stopped(_)))
        })
    }

    pub(super) fn stopped_since(
        &self,
        revision: u64,
        selected: Option<u64>,
    ) -> Option<(u64, String)> {
        self.threads
            .iter()
            .filter(|(id, _)| selected.is_none_or(|selected| **id == selected))
            .filter_map(|(id, _)| {
                self.stop(*id)
                    .filter(|stop| stop.revision > revision)
                    .map(|stop| (*id, stop))
            })
            .max_by_key(|(_, stop)| stop.revision)
            .map(|(id, stop)| (id, stop.reason.clone()))
    }

    pub(super) fn resume(&mut self, thread: u64, single: bool) -> Result<Resume> {
        self.require(Some(thread))?;
        let revision = self.advance()?;
        let ticket = Resume {
            baseline: self.baseline.clone(),
            threads: self.threads.clone(),
            selected: self.selected,
            thread,
            revision,
        };
        let cell = Cell {
            status: Status::Running,
            revision,
        };
        if single {
            self.insert(thread)?.cell = cell;
        } else {
            self.baseline = cell.clone();
            for thread in self.threads.values_mut() {
                thread.cell = cell.clone();
            }
        }
        self.choose();
        Ok(ticket)
    }

    pub(super) fn restore(&mut self, ticket: &Resume, except_resumed: bool) {
        if self.exited {
            return;
        }
        if self.baseline.revision == ticket.revision {
            self.baseline = ticket.baseline.clone();
        }
        for (id, previous) in &ticket.threads {
            if except_resumed && *id == ticket.thread {
                continue;
            }
            if let Some(current) = self.threads.get_mut(id)
                && current.cell.revision == ticket.revision
            {
                current.cell = previous.cell.clone();
            }
        }
        if self.selected.is_none() {
            self.selected = ticket.selected;
        }
        self.choose();
    }

    pub(super) fn continued_all(&mut self, ticket: &Resume) {
        if self.exited {
            return;
        }
        let cell = Cell {
            status: Status::Running,
            revision: ticket.revision,
        };
        if self.baseline.revision <= ticket.revision {
            self.baseline = cell.clone();
        }
        for thread in self.threads.values_mut() {
            if thread.cell.revision <= ticket.revision {
                thread.cell = cell.clone();
            }
        }
        self.choose();
    }
}

fn boolean(body: &Value, key: &str, default: bool) -> Result<bool> {
    body.get(key).map_or(Ok(default), |value| {
        value
            .as_bool()
            .ok_or_else(|| tool_err("DAP_PROTOCOL", format!("{key} must be boolean")))
    })
}

fn thread_id(value: Option<&Value>) -> Result<Option<u64>> {
    value
        .map(|value| {
            value
                .as_u64()
                .filter(|id| (1..=2_147_483_647).contains(id))
                .ok_or_else(|| tool_err("DAP_PROTOCOL", "thread ID must be a positive DAP integer"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped(execution: &mut Execution, id: u64, all: bool) {
        execution
            .event(
                "stopped",
                &json!({"threadId":id,"reason":"breakpoint","allThreadsStopped":all}),
            )
            .unwrap();
    }

    fn two_threads() -> Execution {
        let mut execution = Execution::default();
        stopped(&mut execution, 7, true);
        execution
            .observe_threads(
                &json!({"threads":[{"id":7,"name":"main"},{"id":8,"name":"worker"}]}),
                execution.revision(),
            )
            .unwrap();
        execution
    }

    #[test]
    fn omitted_all_stopped_never_grants_other_threads_inspection() {
        let mut execution = Execution::default();
        execution
            .observe_threads(
                &json!({"threads":[{"id":7,"name":"main"},{"id":8,"name":"worker"}]}),
                0,
            )
            .unwrap();
        execution
            .event("stopped", &json!({"threadId":7,"reason":"exception"}))
            .unwrap();
        assert_eq!(execution.require(Some(7)).unwrap(), 7);
        assert!(execution.require(Some(8)).is_err());
    }

    #[test]
    fn partial_continue_preserves_a_stopped_peer_and_default_continue_resumes_all() {
        let mut execution = two_threads();
        execution
            .event(
                "continued",
                &json!({"threadId":7,"allThreadsContinued":false}),
            )
            .unwrap();
        assert!(execution.require(Some(7)).is_err());
        assert_eq!(execution.require(None).unwrap(), 8);
        execution
            .event("continued", &json!({"threadId":8}))
            .unwrap();
        assert_eq!(execution.aggregate(), ExecState::Running);
    }

    #[test]
    fn late_inventory_uses_all_stop_but_never_resurrects_an_exited_thread() {
        let mut execution = two_threads();
        let revision = execution.revision();
        execution
            .event("thread", &json!({"threadId":8,"reason":"exited"}))
            .unwrap();
        execution
            .observe_threads(&json!({"threads":[{"id":8,"name":"stale"}]}), revision)
            .unwrap();
        assert!(execution.require(Some(8)).is_err());
        execution
            .event("thread", &json!({"threadId":8,"reason":"started"}))
            .unwrap();
        assert!(execution.require(Some(8)).is_err());
        assert_eq!(execution.require(Some(7)).unwrap(), 7);
    }

    #[test]
    fn rejected_single_resume_restores_only_its_own_transition() {
        let mut execution = two_threads();
        let ticket = execution.resume(7, true).unwrap();
        execution
            .event(
                "continued",
                &json!({"threadId":8,"allThreadsContinued":false}),
            )
            .unwrap();
        execution.restore(&ticket, false);
        assert_eq!(execution.require(Some(7)).unwrap(), 7);
        assert!(execution.require(Some(8)).is_err());
    }

    #[test]
    fn early_stop_wins_over_success_and_rejection_replies() {
        for rejected in [false, true] {
            let mut execution = two_threads();
            let revision = execution.revision();
            let ticket = execution.resume(7, true).unwrap();
            stopped(&mut execution, 7, false);
            if rejected {
                execution.restore(&ticket, false);
            } else {
                execution.continued_all(&ticket);
            }
            assert_eq!(execution.stopped_since(revision, Some(7)).unwrap().0, 7);
        }
    }

    #[test]
    fn partial_continue_reply_restores_untouched_peers_not_the_resumed_thread() {
        let mut execution = two_threads();
        let ticket = execution.resume(7, false).unwrap();
        execution.restore(&ticket, true);
        assert!(execution.require(Some(7)).is_err());
        assert_eq!(execution.require(Some(8)).unwrap(), 8);
    }

    #[test]
    fn waiting_for_one_thread_does_not_return_an_old_or_unrelated_stop() {
        let mut execution = two_threads();
        let revision = execution.revision();
        execution.resume(7, true).unwrap();
        assert!(execution.stopped_since(revision, None).is_none());
        stopped(&mut execution, 8, false);
        assert!(execution.stopped_since(revision, Some(7)).is_none());
        stopped(&mut execution, 7, false);
        assert_eq!(execution.stopped_since(revision, Some(7)).unwrap().0, 7);
    }

    #[test]
    fn all_stop_without_thread_requires_inventory_and_invalid_events_fail() {
        let mut execution = Execution::default();
        execution
            .event(
                "stopped",
                &json!({"reason":"pause","allThreadsStopped":true}),
            )
            .unwrap();
        assert_eq!(execution.require(None).unwrap(), 0);
        assert!(execution.require(Some(99)).is_err());
        assert!(
            execution
                .event("stopped", &json!({"reason":"pause"}))
                .is_err()
        );
        assert!(
            execution
                .event("continued", &json!({"allThreadsContinued":false}))
                .is_err()
        );
        assert!(
            execution
                .event(
                    "stopped",
                    &json!({"threadId":7,"reason":"pause","allThreadsStopped":"yes"})
                )
                .is_err()
        );
    }

    #[test]
    fn termination_is_final_and_preserve_focus_keeps_the_existing_stop() {
        let mut execution = two_threads();
        execution
            .event(
                "stopped",
                &json!({"threadId":8,"reason":"exception","preserveFocusHint":true}),
            )
            .unwrap();
        assert_eq!(execution.require(None).unwrap(), 7);
        execution.event("terminated", &json!({})).unwrap();
        stopped(&mut execution, 7, true);
        assert_eq!(execution.aggregate(), ExecState::Exited);
    }

    #[test]
    fn thread_limits_and_duplicate_inventory_are_not_silently_truncated() {
        let mut execution = Execution::default();
        assert!(
            execution
                .observe_threads(
                    &json!({"threads":[{"id":7,"name":"a"},{"id":7,"name":"b"}]}),
                    0
                )
                .is_err()
        );
        for id in 1..=MAX_THREADS as u64 {
            execution
                .event("thread", &json!({"threadId":id,"reason":"started"}))
                .unwrap();
        }
        assert!(
            execution
                .event(
                    "thread",
                    &json!({"threadId":MAX_THREADS + 1,"reason":"started"})
                )
                .is_err()
        );
    }
}
