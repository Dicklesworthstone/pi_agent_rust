//! One-shot model handoff after planning and the first successful file edit.
//!
//! This state only requests a boundary. The SDK owns persistence, model
//! installation and continuation; tool output can never choose a destination.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{ThinkingLevel, ToolCall, ToolResultMessage};

pub const COMMAND_USAGE: &str = "Usage: /prewalk [into] | restart [from] [into] | status | off";

/// Shared syntax for both terminal frontends. Only command keywords are case
/// insensitive; model IDs and role selectors retain their exact spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrewalkCommand<'a> {
    Arm {
        into: Option<&'a str>,
    },
    Restart {
        from: Option<&'a str>,
        into: Option<&'a str>,
    },
    Status,
    Off,
}

impl<'a> PrewalkCommand<'a> {
    pub fn parse(input: &'a str) -> Result<Self, &'static str> {
        let mut words = input.split_whitespace();
        let Some(first) = words.next() else {
            return Ok(Self::Arm { into: None });
        };
        let command = if first.eq_ignore_ascii_case("restart") {
            Self::Restart {
                from: words.next(),
                into: words.next(),
            }
        } else if first.eq_ignore_ascii_case("status") {
            Self::Status
        } else if first.eq_ignore_ascii_case("off") {
            Self::Off
        } else {
            Self::Arm { into: Some(first) }
        };
        if words.next().is_some() {
            return Err(COMMAND_USAGE);
        }
        Ok(command)
    }
}

/// Explicit destination for an OMP-style prewalk. Credentials are resolved by
/// the owning session at admission and again when installing the destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrewalkTarget {
    pub provider: String,
    pub model: String,
    pub thinking: Option<ThinkingLevel>,
}

impl PrewalkTarget {
    #[must_use]
    pub fn label(&self) -> String {
        let mut label = format!("{}/{}", self.provider, self.model);
        if let Some(thinking) = self.thinking {
            label.push(':');
            label.push_str(&thinking.to_string());
        }
        label
    }
}

/// Ownership of a logical turn across native provider attempts.
///
/// Drivers create this once, preserve it through retries, and finish it at the
/// user-request boundary. Session navigation invalidates its captured origin.
pub struct PrewalkTurn {
    pub(crate) origin: crate::extensions::SessionActionOrigin,
}

/// Pure, session-local trigger state. Never restored automatically from disk.
#[derive(Debug, Clone)]
pub(crate) struct PrewalkState {
    pub target: PrewalkTarget,
    todo_ready: bool,
    ready: bool,
    yielded: bool,
}

impl PrewalkState {
    pub const fn new(target: PrewalkTarget, todo_ready: bool) -> Self {
        Self {
            target,
            todo_ready,
            ready: false,
            yielded: false,
        }
    }

    pub const fn ready(&self) -> bool {
        self.ready
    }

    pub const fn yielded(&self) -> bool {
        self.yielded
    }

    pub const fn yield_at_boundary(&mut self) -> bool {
        self.yielded = self.ready();
        self.yielded
    }

    /// A completed batch is the smallest handoff unit: every call already
    /// has a result, including rejected/skipped calls. Successful todo results
    /// carry the actual list, not a model's assertion that it wrote one.
    pub fn observe(&mut self, calls: &[ToolCall], results: &[Arc<ToolResultMessage>]) {
        for result in results.iter().filter(|result| !result.is_error) {
            if result.tool_name == "todo"
                && let Some(details) = &result.details
                && details.get("schema").and_then(serde_json::Value::as_str)
                    == Some(crate::todo::TODO_LIST_SCHEMA)
                && let Some(list) = details.get("list")
                && let Ok(list) = serde_json::from_value::<crate::todo::TodoList>(list.clone())
            {
                self.todo_ready = !list.is_empty();
            }
        }
        if !self.todo_ready {
            return;
        }
        self.ready |= calls.iter().any(|call| {
            matches!(call.name.as_str(), "write" | "edit" | "hashline_edit")
                && results.iter().any(|result| {
                    result.tool_call_id == call.id
                        && result.tool_name == call.name
                        && !result.is_error
                        && (call
                            .arguments
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(project_path)
                            || (call.name == "write" && conflict_write_changed_project(result)))
                })
        });
    }
}

fn conflict_write_changed_project(result: &ToolResultMessage) -> bool {
    let Some(details) = &result.details else {
        return false;
    };
    details.get("schema").and_then(serde_json::Value::as_str) == Some("pi.conflict.write.v1")
        && details
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|paths| {
                paths
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(project_path)
            })
}

fn project_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.contains("://") {
        return false;
    }
    // A plan artifact is preparation, not the start of implementation.
    let normalized = path.replace('\\', "/");
    !normalized
        .split('/')
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| matches!(pair[0], ".pi" | ".omp") && pair[1] == "plans")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_selectors_preserve_case_and_reject_extra_arguments() {
        assert_eq!(
            PrewalkCommand::parse("  "),
            Ok(PrewalkCommand::Arm { into: None })
        );
        assert_eq!(
            PrewalkCommand::parse("Provider/Model:high"),
            Ok(PrewalkCommand::Arm {
                into: Some("Provider/Model:high")
            })
        );
        assert_eq!(
            PrewalkCommand::parse("ReStArT @slow Provider/Model:low"),
            Ok(PrewalkCommand::Restart {
                from: Some("@slow"),
                into: Some("Provider/Model:low"),
            })
        );
        assert_eq!(
            PrewalkCommand::parse("restart @plan"),
            Ok(PrewalkCommand::Restart {
                from: Some("@plan"),
                into: None
            })
        );
        assert_eq!(PrewalkCommand::parse("STATUS"), Ok(PrewalkCommand::Status));
        assert_eq!(PrewalkCommand::parse("Off"), Ok(PrewalkCommand::Off));
        for invalid in [
            "status extra",
            "off @smol",
            "@smol extra",
            "restart @slow @smol extra",
        ] {
            assert_eq!(
                PrewalkCommand::parse(invalid),
                Err(COMMAND_USAGE),
                "{invalid}"
            );
        }
    }

    fn target() -> PrewalkTarget {
        PrewalkTarget {
            provider: "openai".into(),
            model: "target".into(),
            thinking: None,
        }
    }

    fn call(path: &str) -> ToolCall {
        ToolCall {
            id: "edit-1".into(),
            name: "write".into(),
            arguments: json!({"path": path, "content": "done"}),
            thought_signature: None,
        }
    }

    fn result(call: &ToolCall, failed: bool) -> Arc<ToolResultMessage> {
        Arc::new(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: Vec::new(),
            details: None,
            is_error: failed,
            timestamp: 0,
        })
    }

    #[test]
    fn only_a_successful_project_edit_after_todos_requests_handoff() {
        let edit = call("src/parser.rs");
        let mut state = PrewalkState::new(target(), false);
        state.observe(std::slice::from_ref(&edit), &[result(&edit, false)]);
        assert!(!state.ready());
        let todo = Arc::new(ToolResultMessage {
            tool_call_id: "plan-1".into(),
            tool_name: "todo".into(),
            content: Vec::new(),
            details: Some(json!({"schema":crate::todo::TODO_LIST_SCHEMA,
                "list":{"phases":[{"tasks":[{"content":"implement parser", "state":"in_progress"}]}]}})),
            is_error: false,
            timestamp: 0,
        });
        state.observe(&[], &[todo]);
        state.observe(std::slice::from_ref(&edit), &[result(&edit, true)]);
        assert!(!state.ready());
        for path in [
            "plan://current",
            ".pi/plans/change.md",
            ".omp/plans/change.md",
        ] {
            let preparation = call(path);
            state.observe(
                std::slice::from_ref(&preparation),
                &[result(&preparation, false)],
            );
            assert!(!state.ready());
        }
        state.observe(std::slice::from_ref(&edit), &[result(&edit, false)]);
        assert!(state.ready());
    }

    #[test]
    fn missing_todo_tool_allows_the_first_edit_and_mismatched_results_do_not() {
        let edit = call("src/parser.rs");
        let mut state = PrewalkState::new(target(), true);
        let mut other = result(&edit, false);
        Arc::make_mut(&mut other).tool_call_id = "different-call".into();
        state.observe(std::slice::from_ref(&edit), &[other]);
        assert!(!state.ready());
        state.observe(std::slice::from_ref(&edit), &[result(&edit, false)]);
        assert!(state.ready());
    }

    #[test]
    fn native_conflict_resolution_uses_the_applied_files_from_its_result() {
        let edit = call("conflict://*");
        let mut state = PrewalkState::new(target(), true);
        state.observe(std::slice::from_ref(&edit), &[result(&edit, false)]);
        assert!(!state.ready());
        let mut resolved = result(&edit, false);
        Arc::make_mut(&mut resolved).details = Some(json!({
            "schema":"pi.conflict.write.v1", "paths":["src/parser.rs"]
        }));
        state.observe(std::slice::from_ref(&edit), &[resolved]);
        assert!(state.ready());
    }
}
