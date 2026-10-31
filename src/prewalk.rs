//! One-shot model handoff after planning and the first successful file edit.
//!
//! This state only requests a boundary. The SDK owns persistence, model
//! installation and continuation; tool output can never choose a destination.

use serde::{Deserialize, Serialize};

use crate::model::ThinkingLevel;

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

/// Native execution evidence, never deserialized from model or extension data.
///
/// Only opted-in host tools can construct nonempty evidence. The Agent captures
/// it before result hooks and merges nested calls into the enclosing batch;
/// neither a guest's return value nor a tool name grants this authority.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolEvidence {
    todo_ready: Option<bool>,
    project_changed: bool,
}

impl ToolEvidence {
    pub(crate) fn todo(output: &crate::tools::ToolOutput) -> Self {
        let todo_ready = output.details.as_ref().and_then(|details| {
            if details.get("schema").and_then(serde_json::Value::as_str)
                != Some(crate::todo::TODO_LIST_SCHEMA)
            {
                return None;
            }
            serde_json::from_value::<crate::todo::TodoList>(details.get("list")?.clone())
                .ok()
                .map(|list| !list.is_empty())
        });
        Self {
            todo_ready,
            project_changed: false,
        }
    }

    pub(crate) fn paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            todo_ready: None,
            project_changed: paths.into_iter().any(project_path),
        }
    }

    pub(crate) fn text_edit(input: &serde_json::Value, output: &crate::tools::ToolOutput) -> Self {
        // Local edit/hashline operations reject identical replacements before
        // success. Editor-mediated writes additionally report the observed
        // final image: an acknowledgment alone cannot prove a mutation.
        if output
            .details
            .as_ref()
            .and_then(|details| details.get("changed"))
            .is_some_and(|changed| changed != true)
        {
            return Self::default();
        }
        Self::paths(input.get("path").and_then(serde_json::Value::as_str))
    }

    pub(crate) fn merge(&mut self, other: Self) {
        if other.todo_ready.is_some() {
            self.todo_ready = other.todo_ready;
        }
        self.project_changed |= other.project_changed;
    }
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

    /// A completed batch is the smallest handoff unit, including all native
    /// calls made by eval or xdev. Evidence is consumed only after its outer
    /// calls have results; a nested mutation never switches a running tool.
    pub fn observe(&mut self, evidence: ToolEvidence) {
        if let Some(ready) = evidence.todo_ready {
            self.todo_ready = ready;
        }
        self.ready |= self.todo_ready && evidence.project_changed;
    }
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

    #[test]
    fn only_a_successful_project_edit_after_todos_requests_handoff() {
        let mut state = PrewalkState::new(target(), false);
        state.observe(ToolEvidence::paths(["src/parser.rs"]));
        assert!(!state.ready());
        let todo = crate::tools::ToolOutput {
            content: Vec::new(),
            details: Some(json!({"schema":crate::todo::TODO_LIST_SCHEMA,
                "list":{"phases":[{"tasks":[{"content":"implement parser", "state":"in_progress"}]}]}})),
            is_error: false,
        };
        state.observe(ToolEvidence::todo(&todo));
        state.observe(ToolEvidence::default());
        assert!(!state.ready());
        for path in [
            "plan://current",
            ".pi/plans/change.md",
            ".omp/plans/change.md",
            "out://",
            "OUT://progress",
        ] {
            state.observe(ToolEvidence::paths([path]));
            assert!(!state.ready());
        }
        state.observe(ToolEvidence::paths(["src/parser.rs"]));
        assert!(state.ready());
    }

    #[test]
    fn missing_todo_tool_allows_the_first_native_edit() {
        let mut state = PrewalkState::new(target(), true);
        state.observe(ToolEvidence::default());
        assert!(!state.ready());
        state.observe(ToolEvidence::paths(["src/parser.rs"]));
        assert!(state.ready());
    }

    #[test]
    fn a_batch_uses_the_latest_native_todo_state_before_its_handoff() {
        let mut state = PrewalkState::new(target(), false);
        let mut batch = ToolEvidence::paths(["src/parser.rs"]);
        batch.merge(ToolEvidence {
            todo_ready: Some(true),
            project_changed: false,
        });
        batch.merge(ToolEvidence {
            todo_ready: Some(false),
            project_changed: false,
        });
        state.observe(batch);
        assert!(!state.ready());
        batch.merge(ToolEvidence {
            todo_ready: Some(true),
            project_changed: false,
        });
        state.observe(batch);
        assert!(state.ready());
    }
}
