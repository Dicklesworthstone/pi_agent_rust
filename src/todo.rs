//! Session todo list: state machine + tool (bd-cv653.3.9).
//!
//! Maintains the session's ordered task list with phase tracking and
//! auto-promotion, mirroring omp's `todo` tool semantics:
//!
//! - Ordered mutations: `init` / `start` / `done` / `drop` / `block` /
//!   `unblock` / `rm` / `append` / `view`.
//! - Task content is verbatim — tasks are addressed by their exact content
//!   string, never by generated ids.
//! - Completed tasks never revert.
//! - Each completion auto-promotes the earliest still-open task (in phase
//!   order) to in-progress.
//!
//! State persists as a session `Custom` entry (`todo_list.v1`) on every
//! mutation, so it survives `--continue`, branches with the session tree,
//! and replays. Rendering surfaces (TUI footer, RPC/ACP clients) are
//! state-driven: they consume the same `todo_list.v1` payload carried in the
//! tool result's `details`, never bespoke side channels.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Schema identifier for the persisted state and the tool-result details.
pub const TODO_LIST_SCHEMA: &str = "todo_list.v1";

/// Task status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum TodoStatus {
    Open,
    InProgress,
    Done,
    Dropped,
    Blocked { reason: String },
}

impl TodoStatus {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Dropped)
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Dropped => "dropped",
            Self::Blocked { .. } => "blocked",
        }
    }
}

/// One task; content is verbatim caller input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoTask {
    pub content: String,
    #[serde(flatten)]
    pub status: TodoStatus,
}

/// One phase: an optional name plus ordered tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoPhase {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub tasks: Vec<TodoTask>,
}

/// The whole todo list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoList {
    pub phases: Vec<TodoPhase>,
}

impl TodoList {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.phases.iter().all(|phase| phase.tasks.is_empty())
    }

    /// All tasks in phase order.
    fn tasks_mut(&mut self) -> impl Iterator<Item = &mut TodoTask> {
        self.phases.iter_mut().flat_map(|phase| &mut phase.tasks)
    }

    fn tasks(&self) -> impl Iterator<Item = &TodoTask> {
        self.phases.iter().flat_map(|phase| &phase.tasks)
    }

    /// Locate a task by verbatim content. Errors on unknown content so a
    /// mistyped reference never mutates state.
    fn find_mut(&mut self, content: &str) -> Result<&mut TodoTask> {
        self.tasks_mut()
            .find(|task| task.content == content)
            .ok_or_else(|| {
                Error::validation(format!(
                    "unknown task {content:?}; tasks are addressed by their exact content"
                ))
            })
    }

    /// The currently in-progress task, if any.
    #[must_use]
    pub fn current(&self) -> Option<&TodoTask> {
        self.tasks()
            .find(|task| matches!(task.status, TodoStatus::InProgress))
    }

    /// Counts: (done+dropped, total).
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        let mut settled = 0usize;
        let mut total = 0usize;
        for task in self.tasks() {
            total += 1;
            if task.status.is_terminal() {
                settled += 1;
            }
        }
        (settled, total)
    }

    /// Promote the earliest still-open task to in-progress when nothing is
    /// currently in progress.
    fn auto_promote(&mut self) {
        if self.current().is_some() {
            return;
        }
        if let Some(task) = self
            .tasks_mut()
            .find(|task| matches!(task.status, TodoStatus::Open))
        {
            task.status = TodoStatus::InProgress;
        }
    }

    /// Compact single-line summary for footer surfaces:
    /// `3/7 · current task content`.
    #[must_use]
    pub fn summary_line(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let (settled, total) = self.progress();
        let current = self.current().map_or_else(
            || "(no task in progress)".to_string(),
            |task| task.content.clone(),
        );
        Some(format!("{settled}/{total} · {current}"))
    }

    /// Multi-line rendering for the `view` op and tool result text.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        if self.is_empty() {
            return "(todo list is empty)".to_string();
        }
        let mut out = String::new();
        for (index, phase) in self.phases.iter().enumerate() {
            if let Some(name) = &phase.name {
                let _ = writeln!(out, "## {name}");
            } else if self.phases.len() > 1 {
                let _ = writeln!(out, "## phase {}", index + 1);
            }
            for task in &phase.tasks {
                match &task.status {
                    TodoStatus::Open => {
                        let _ = writeln!(out, "[ ] {}", task.content);
                    }
                    TodoStatus::InProgress => {
                        let _ = writeln!(out, "[>] {}", task.content);
                    }
                    TodoStatus::Done => {
                        let _ = writeln!(out, "[x] {}", task.content);
                    }
                    TodoStatus::Dropped => {
                        let _ = writeln!(out, "[-] {}", task.content);
                    }
                    TodoStatus::Blocked { reason } => {
                        let _ = writeln!(out, "[!] ({reason}) {}", task.content);
                    }
                }
            }
        }
        let (settled, total) = self.progress();
        let _ = write!(out, "\n{settled}/{total} settled");
        out
    }
}

/// One mutation op, as accepted by the tool input.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum TodoOp {
    /// Replace the list. `phases` for a phased list, or `tasks` for a flat
    /// one (exactly one must be provided).
    Init {
        #[serde(default)]
        phases: Option<Vec<TodoInitPhase>>,
        #[serde(default)]
        tasks: Option<Vec<String>>,
    },
    Start {
        task: String,
    },
    Done {
        task: String,
    },
    Drop {
        task: String,
    },
    Block {
        task: String,
        reason: String,
    },
    Unblock {
        task: String,
    },
    Rm {
        task: String,
    },
    /// Append tasks to a phase (by name; created if absent). Without
    /// `phase`, appends to the last phase.
    Append {
        #[serde(default)]
        phase: Option<String>,
        tasks: Vec<String>,
    },
    View,
}

/// Phase shape accepted by `init`.
#[derive(Debug, Clone, Deserialize)]
pub struct TodoInitPhase {
    #[serde(default)]
    pub name: Option<String>,
    pub tasks: Vec<String>,
}

fn non_empty_tasks(tasks: &[String], op: &str) -> Result<()> {
    if tasks.is_empty() {
        return Err(Error::validation(format!(
            "{op} requires at least one task"
        )));
    }
    if let Some(blank) = tasks.iter().find(|task| task.trim().is_empty()) {
        let _ = blank;
        return Err(Error::validation(format!("{op} tasks must not be blank")));
    }
    Ok(())
}

/// Apply `op` to `list`. On error the list is untouched (ops validate before
/// mutating). Returns `true` when state changed (i.e. should persist).
#[allow(clippy::too_many_lines)]
pub fn apply_op(list: &mut TodoList, op: &TodoOp) -> Result<bool> {
    match op {
        TodoOp::Init { phases, tasks } => {
            let next = match (phases, tasks) {
                (Some(phases), None) => {
                    if phases.is_empty() {
                        return Err(Error::validation("init requires at least one phase"));
                    }
                    for phase in phases {
                        non_empty_tasks(&phase.tasks, "init")?;
                    }
                    TodoList {
                        phases: phases
                            .iter()
                            .map(|phase| TodoPhase {
                                name: phase.name.clone(),
                                tasks: phase
                                    .tasks
                                    .iter()
                                    .map(|content| TodoTask {
                                        content: content.clone(),
                                        status: TodoStatus::Open,
                                    })
                                    .collect(),
                            })
                            .collect(),
                    }
                }
                (None, Some(tasks)) => {
                    non_empty_tasks(tasks, "init")?;
                    TodoList {
                        phases: vec![TodoPhase {
                            name: None,
                            tasks: tasks
                                .iter()
                                .map(|content| TodoTask {
                                    content: content.clone(),
                                    status: TodoStatus::Open,
                                })
                                .collect(),
                        }],
                    }
                }
                _ => {
                    return Err(Error::validation(
                        "init requires exactly one of `phases` or `tasks`",
                    ));
                }
            };
            *list = next;
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Start { task } => {
            {
                let found = list.find_mut(task)?;
                if found.status.is_terminal() {
                    return Err(Error::validation(format!(
                        "task {task:?} is {} and completed tasks never revert",
                        found.status.label()
                    )));
                }
            }
            // Deliberate pointer semantics: starting a task demotes any other
            // in-progress task back to open (one task in progress at a time).
            for other in list.tasks_mut() {
                if matches!(other.status, TodoStatus::InProgress) {
                    other.status = TodoStatus::Open;
                }
            }
            list.find_mut(task)?.status = TodoStatus::InProgress;
            Ok(true)
        }
        TodoOp::Done { task } => {
            let found = list.find_mut(task)?;
            if matches!(found.status, TodoStatus::Dropped) {
                return Err(Error::validation(format!(
                    "task {task:?} is dropped and completed tasks never revert"
                )));
            }
            found.status = TodoStatus::Done;
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Drop { task } => {
            let found = list.find_mut(task)?;
            if matches!(found.status, TodoStatus::Done) {
                return Err(Error::validation(format!(
                    "task {task:?} is done and completed tasks never revert"
                )));
            }
            found.status = TodoStatus::Dropped;
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Block { task, reason } => {
            if reason.trim().is_empty() {
                return Err(Error::validation("block requires a non-empty reason"));
            }
            let found = list.find_mut(task)?;
            if found.status.is_terminal() {
                return Err(Error::validation(format!(
                    "task {task:?} is {} and completed tasks never revert",
                    found.status.label()
                )));
            }
            found.status = TodoStatus::Blocked {
                reason: reason.clone(),
            };
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Unblock { task } => {
            let found = list.find_mut(task)?;
            let TodoStatus::Blocked { .. } = &found.status else {
                return Err(Error::validation(format!("task {task:?} is not blocked")));
            };
            found.status = TodoStatus::Open;
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Rm { task } => {
            // Validate existence first (no state change on unknown task).
            let _ = list.find_mut(task)?;
            for phase in &mut list.phases {
                phase.tasks.retain(|candidate| candidate.content != *task);
            }
            list.auto_promote();
            Ok(true)
        }
        TodoOp::Append { phase, tasks } => {
            non_empty_tasks(tasks, "append")?;
            let new_tasks = tasks.iter().map(|content| TodoTask {
                content: content.clone(),
                status: TodoStatus::Open,
            });
            match phase {
                Some(name) => {
                    if let Some(existing) = list
                        .phases
                        .iter_mut()
                        .find(|candidate| candidate.name.as_deref() == Some(name.as_str()))
                    {
                        existing.tasks.extend(new_tasks);
                    } else {
                        list.phases.push(TodoPhase {
                            name: Some(name.clone()),
                            tasks: new_tasks.collect(),
                        });
                    }
                }
                None => {
                    if let Some(last) = list.phases.last_mut() {
                        last.tasks.extend(new_tasks);
                    } else {
                        list.phases.push(TodoPhase {
                            name: None,
                            tasks: new_tasks.collect(),
                        });
                    }
                }
            }
            list.auto_promote();
            Ok(true)
        }
        TodoOp::View => Ok(false),
    }
}

/// Extract the latest persisted todo list from session entries along the
/// current path (last `todo_list.v1` custom entry wins).
#[must_use]
pub fn latest_from_entries<'a, I>(entries: I) -> TodoList
where
    I: IntoIterator<Item = &'a crate::session::SessionEntry>,
{
    let mut latest = TodoList::default();
    for entry in entries {
        if let crate::session::SessionEntry::Custom(custom) = entry
            && custom.custom_type == TODO_LIST_SCHEMA
            && let Some(data) = &custom.data
            && let Ok(list) = TodoList::deserialize(data)
        {
            latest = list;
        }
    }
    latest
}

/// The `todo` tool: session task list with phase tracking (bd-cv653.3.9).
///
/// State loads lazily from the session's current path (latest `todo_list.v1`
/// custom entry) and every mutation appends a fresh entry, so the list
/// persists across `--continue`, forks with the session tree, and replays.
/// The tool result's `details` always carries the full state under the same
/// schema, so RPC/ACP clients and the TUI footer render from one source.
pub struct TodoTool {
    session: std::sync::Arc<asupersync::sync::Mutex<crate::session::Session>>,
}

impl TodoTool {
    #[must_use]
    pub const fn new(
        session: std::sync::Arc<asupersync::sync::Mutex<crate::session::Session>>,
    ) -> Self {
        Self { session }
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::tools::Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn label(&self) -> &str {
        "Todo"
    }

    fn description(&self) -> &str {
        "Maintain the session task list. Ops: init (phases or flat tasks), start, done, drop, block (with reason), unblock, rm, append, view. Tasks are addressed by their EXACT content string (verbatim; no ids). Completing a task auto-promotes the earliest still-open task to in_progress; completed tasks never revert; out-of-order completion is fine — the pointer stays on the earliest open task. State persists with the session and forks with branches."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["init", "start", "done", "drop", "block", "unblock", "rm", "append", "view"],
                    "description": "Mutation to apply."
                },
                "phases": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["tasks"],
                        "properties": {
                            "name": {"type": "string"},
                            "tasks": {"type": "array", "items": {"type": "string"}}
                        }
                    },
                    "description": "init: phased list (exactly one of phases/tasks)."
                },
                "tasks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "init: flat list. append: tasks to add."
                },
                "task": {"type": "string", "description": "Exact content of the task to mutate."},
                "reason": {"type": "string", "description": "block: why the task is blocked."},
                "phase": {"type": "string", "description": "append: phase name (created if absent; defaults to the last phase)."}
            },
            "required": ["op"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> crate::tools::ToolEffects {
        // Session-state mutation only; no filesystem or process effects.
        crate::tools::ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
    ) -> Result<crate::tools::ToolOutput> {
        let op: TodoOp = serde_json::from_value(input)
            .map_err(|error| Error::validation(format!("Invalid todo input: {error}")))?; // ubs:ignore false positive: cold error path, not a loop allocation

        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|error| Error::session(format!("Failed to lock session: {error}")))?; // ubs:ignore false positive: cold error path, not a loop allocation

        let mut list = latest_from_entries(session.entries_for_current_path());
        let mutated = apply_op(&mut list, &op)?;
        let state = serde_json::to_value(&list)
            .map_err(|error| Error::session(format!("todo state serialize: {error}")))?; // ubs:ignore false positive: cold error path, not a loop allocation
        if mutated {
            let persisted = serde_json::to_value(&list)
                .map_err(|error| Error::session(format!("todo state serialize: {error}")))?; // ubs:ignore false positive: cold error path, not a loop allocation
            session.append_custom_entry(TODO_LIST_SCHEMA.to_string(), Some(persisted));
        }
        drop(session);

        Ok(crate::tools::ToolOutput {
            content: vec![crate::model::ContentBlock::Text(
                crate::model::TextContent::new(list.render()),
            )],
            details: Some(serde_json::json!({
                "schema": TODO_LIST_SCHEMA,
                "list": state,
                "summary": list.summary_line(),
            })),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(tasks: &[&str]) -> TodoList {
        let mut list = TodoList::default();
        let op = TodoOp::Init {
            phases: None,
            tasks: Some(tasks.iter().map(|task| (*task).to_string()).collect()),
        };
        apply_op(&mut list, &op).expect("init"); // ubs:ignore test helper
        list
    }

    fn op(json: serde_json::Value) -> TodoOp {
        serde_json::from_value(json).expect("parse op")
    }

    /// init auto-promotes the first task; done promotes the next open task
    /// in phase order.
    #[test]
    fn init_and_done_auto_promote_in_phase_order() {
        let mut list = TodoList::default();
        apply_op(
            &mut list,
            &op(serde_json::json!({
                "op": "init",
                "phases": [
                    {"name": "plan", "tasks": ["write spec", "review spec"]},
                    {"name": "build", "tasks": ["implement"]},
                ]
            })),
        )
        .expect("init phased");
        assert_eq!(list.current().expect("current").content, "write spec");

        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "write spec"})),
        )
        .expect("done");
        assert_eq!(list.current().expect("current").content, "review spec");

        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "review spec"})),
        )
        .expect("done 2");
        assert_eq!(list.current().expect("current").content, "implement");
        assert_eq!(list.progress(), (2, 3));
    }

    /// Out-of-order completion is allowed; the pointer stays on the earliest
    /// open task.
    #[test]
    fn out_of_order_completion_keeps_pointer_semantics() {
        let mut list = flat(&["a", "b", "c"]);
        assert_eq!(list.current().expect("current").content, "a");
        // Complete c out of order; a remains in progress.
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "c"})),
        )
        .expect("done c");
        assert_eq!(list.current().expect("current").content, "a");
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "a"})),
        )
        .expect("done a");
        assert_eq!(list.current().expect("current").content, "b");
    }

    /// Completed tasks never revert: start/drop on done, start/done on
    /// dropped, block on done all error without state change.
    #[test]
    fn completed_tasks_never_revert() {
        let mut list = flat(&["a", "b"]);
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "a"})),
        )
        .expect("done a");
        for bad in [
            serde_json::json!({"op": "start", "task": "a"}),
            serde_json::json!({"op": "drop", "task": "a"}),
            serde_json::json!({"op": "block", "task": "a", "reason": "why"}),
        ] {
            let before = serde_json::to_value(&list).expect("snapshot");
            let error = apply_op(&mut list, &op(bad)).expect_err("must not revert");
            assert!(error.to_string().contains("never revert"), "{error}");
            assert_eq!(
                serde_json::to_value(&list).expect("snapshot"),
                before,
                "failed op must not mutate state"
            );
        }
    }

    /// block parks the current task (auto-promoting the next), unblock
    /// returns it to open.
    #[test]
    fn block_and_unblock_flow() {
        let mut list = flat(&["a", "b"]);
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "block", "task": "a", "reason": "waiting on CI"})),
        )
        .expect("block");
        assert_eq!(list.current().expect("current").content, "b");
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "unblock", "task": "a"})),
        )
        .expect("unblock");
        // b stays in progress (unblock does not steal the pointer).
        assert_eq!(list.current().expect("current").content, "b");
        let rendered = list.render();
        assert!(rendered.contains("[>] b"));
        assert!(rendered.contains("[ ] a"));
    }

    /// Unknown task content errors and leaves state untouched (acceptance 4).
    #[test]
    fn unknown_task_errors_without_state_change() {
        let mut list = flat(&["a"]);
        let before = serde_json::to_value(&list).expect("snapshot");
        for bad in ["start", "done", "drop", "unblock", "rm"] {
            let error = apply_op(
                &mut list,
                &op(serde_json::json!({"op": bad, "task": "missing"})),
            )
            .expect_err("unknown task");
            assert!(error.to_string().contains("unknown task"), "{error}"); // ubs:ignore test assertion
        }
        assert_eq!(serde_json::to_value(&list).expect("snapshot"), before);
    }

    /// rm removes verbatim-matched tasks; append targets phases by name and
    /// creates them when absent.
    #[test]
    fn rm_and_append_semantics() {
        let mut list = flat(&["a", "b"]);
        apply_op(&mut list, &op(serde_json::json!({"op": "rm", "task": "a"}))).expect("rm");
        assert_eq!(list.current().expect("current").content, "b");

        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "append", "phase": "later", "tasks": ["c"]})),
        )
        .expect("append new phase");
        assert_eq!(list.phases.len(), 2);
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "append", "phase": "later", "tasks": ["d"]})),
        )
        .expect("append existing phase");
        let later = list.phases.last().expect("later phase");
        assert_eq!(later.name.as_deref(), Some("later"));
        assert_eq!(later.tasks.len(), 2);
    }

    /// Persistence round trip through the todo_list.v1 payload shape.
    #[test]
    fn serde_round_trip_preserves_state() {
        let mut list = flat(&["a", "b"]);
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "block", "task": "b", "reason": "deps"})),
        )
        .expect("block");
        let value = serde_json::to_value(&list).expect("serialize");
        let restored: TodoList = serde_json::from_value(value).expect("deserialize");
        assert_eq!(
            serde_json::to_value(&restored).expect("re-serialize"),
            serde_json::to_value(&list).expect("serialize again")
        );
        assert!(restored.render().contains("[!] (deps) b"));
    }

    /// Summary line for footer surfaces.
    #[test]
    fn summary_line_shape() {
        assert!(TodoList::default().summary_line().is_none());
        let mut list = flat(&["a", "b"]);
        apply_op(
            &mut list,
            &op(serde_json::json!({"op": "done", "task": "a"})),
        )
        .expect("done");
        assert_eq!(list.summary_line().expect("summary"), "1/2 · b");
    }

    /// Acceptance 1 + 4 at the tool layer: full lifecycle via the tool with
    /// stable details schema; invalid op errors without appending state.
    #[test]
    fn tool_lifecycle_persists_and_rejects_invalid_ops() {
        use crate::tools::Tool as _;

        asupersync::test_utils::run_test(|| async {
            let session = std::sync::Arc::new(asupersync::sync::Mutex::new(
                crate::session::Session::create(),
            ));
            let tool = TodoTool::new(std::sync::Arc::clone(&session));

            let output = tool
                .execute(
                    "todo-1",
                    serde_json::json!({
                        "op": "init",
                        "phases": [
                            {"name": "plan", "tasks": ["write spec"]},
                            {"name": "build", "tasks": ["implement"]},
                        ]
                    }),
                    None,
                )
                .await
                .expect("init");
            let details = output.details.expect("details");
            assert_eq!(details["schema"], TODO_LIST_SCHEMA);
            assert_eq!(details["summary"], "0/2 · write spec");

            let output = tool
                .execute(
                    "todo-2",
                    serde_json::json!({"op": "done", "task": "write spec"}),
                    None,
                )
                .await
                .expect("done");
            assert_eq!(
                output.details.expect("details")["summary"],
                "1/2 · implement"
            );

            // Unknown task: tool error, no new session entry.
            let cx = crate::agent_cx::AgentCx::for_request();
            let entries_before = {
                let guard = session.lock(cx.cx()).await.expect("lock session");
                guard.entries_for_current_path().len()
            };
            let error = tool
                .execute(
                    "todo-3",
                    serde_json::json!({"op": "done", "task": "not a task"}),
                    None,
                )
                .await
                .expect_err("unknown task errors");
            assert!(error.to_string().contains("unknown task"), "{error}");
            {
                let guard = session.lock(cx.cx()).await.expect("lock session");
                assert_eq!(
                    guard.entries_for_current_path().len(),
                    entries_before,
                    "failed op must not append state"
                );
            }

            // `view` reads without appending.
            let view = tool
                .execute("todo-4", serde_json::json!({"op": "view"}), None)
                .await
                .expect("view");
            let rendered = view
                .content
                .first()
                .and_then(|block| match block {
                    crate::model::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .expect("view must render text");
            assert!(rendered.contains("[x] write spec"));
            assert!(rendered.contains("[>] implement"));
            let guard = session.lock(cx.cx()).await.expect("lock session");
            assert_eq!(guard.entries_for_current_path().len(), entries_before);
        });
    }

    /// Acceptance 2: state survives save + reopen (replay from the
    /// todo_list.v1 custom entries).
    #[test]
    fn state_survives_session_save_and_reopen() {
        use crate::tools::Tool as _;

        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("todo-session.jsonl");
            let mut created = crate::session::Session::create();
            created.path = Some(path.clone());
            let session = std::sync::Arc::new(asupersync::sync::Mutex::new(created));
            let tool = TodoTool::new(std::sync::Arc::clone(&session));

            tool.execute(
                "todo-1",
                serde_json::json!({"op": "init", "tasks": ["a", "b"]}),
                None,
            )
            .await
            .expect("init");
            tool.execute(
                "todo-2",
                serde_json::json!({"op": "done", "task": "a"}),
                None,
            )
            .await
            .expect("done");

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("lock session");
                guard.save().await.expect("save session");
            }

            let reopened = crate::session::Session::open(path.to_string_lossy().as_ref())
                .await
                .expect("reopen session");
            let restored = latest_from_entries(reopened.entries_for_current_path());
            assert_eq!(restored.summary_line().expect("summary"), "1/2 · b");
        });
    }
}
