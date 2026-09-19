//! Client-owned breakpoint sets. DAP set*Breakpoints requests replace whole
//! sets, so individual tool operations must resend the retained configuration.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use asupersync::sync::OwnedMutexGuard;
use serde::Deserialize;
use serde_json::{Value, json};

use super::session::DapSession;
use super::tool_err;
use crate::agent_cx::AgentCx;
use crate::error::Result;

const MAX_GROUPS: usize = 256;
const MAX_BREAKPOINTS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    Source(String),
    Function,
    Instruction,
    Data,
}

impl Group {
    const fn command(&self) -> &'static str {
        match self {
            Self::Source(_) => "setBreakpoints",
            Self::Function => "setFunctionBreakpoints",
            Self::Instruction => "setInstructionBreakpoints",
            Self::Data => "setDataBreakpoints",
        }
    }

    const fn capability(&self) -> Option<&'static str> {
        match self {
            Self::Source(_) => None,
            Self::Function => Some("supportsFunctionBreakpoints"),
            Self::Instruction => Some("supportsInstructionBreakpoints"),
            Self::Data => Some("supportsDataBreakpoints"),
        }
    }

    fn arguments(&self, entries: &[Value]) -> Value {
        let mut args = json!({"breakpoints": entries});
        if let Self::Source(path) = self {
            args["source"] = json!({"path": path});
        }
        args
    }
}

#[derive(Clone, Default)]
struct Set {
    requested: Vec<Value>,
    acknowledged: Vec<Value>,
    synchronized: bool,
}

#[derive(Default)]
pub(super) struct Store {
    groups: BTreeMap<Group, Set>,
}

pub(super) enum Change {
    Upsert(Value),
    Remove(Option<Value>),
    Replace(Vec<Value>),
}

fn same_key(group: &Group, left: &Value, right: &Value) -> bool {
    match group {
        Group::Source(_) => {
            left["line"] == right["line"] && left.get("column") == right.get("column")
        }
        Group::Function => left["name"] == right["name"],
        Group::Instruction => {
            left["instructionReference"] == right["instructionReference"]
                && left.get("offset").and_then(Value::as_i64).unwrap_or(0)
                    == right.get("offset").and_then(Value::as_i64).unwrap_or(0)
        }
        Group::Data => left["dataId"] == right["dataId"],
    }
}

fn proposed(group: &Group, previous: &[Value], change: Change) -> (Vec<Value>, Option<usize>) {
    let mut next = previous.to_vec();
    let selected = match change {
        Change::Upsert(spec) => {
            let index = next.iter().position(|old| same_key(group, old, &spec));
            if let Some(index) = index {
                next[index] = spec;
                Some(index)
            } else {
                next.push(spec);
                Some(next.len() - 1)
            }
        }
        Change::Remove(Some(key)) => {
            next.retain(|old| !same_key(group, old, &key));
            None
        }
        Change::Remove(None) => {
            next.clear();
            None
        }
        Change::Replace(entries) => {
            next = entries;
            None
        }
    };
    (next, selected)
}

pub(super) async fn apply(session: &DapSession, group: Group, change: Change) -> Result<Value> {
    if let Some(capability) = group.capability() {
        session.require_capability(capability)?;
    }
    let owner = AgentCx::for_current_or_request();
    let mut store = OwnedMutexGuard::lock(Arc::clone(&session.breakpoints), owner.cx())
        .await
        .map_err(|_| tool_err("DAP_CANCELLED", "breakpoint update cancelled"))?;
    let previous = store.groups.get(&group).cloned().unwrap_or_default();
    let (next, selected) = proposed(&group, &previous.requested, change);
    let total: usize = store.groups.values().map(|set| set.requested.len()).sum();
    if total - previous.requested.len() + next.len() > MAX_BREAKPOINTS
        || (!store.groups.contains_key(&group) && store.groups.len() >= MAX_GROUPS)
    {
        return Err(tool_err(
            "DAP_BREAKPOINT_LIMIT",
            "breakpoint configuration exceeds session limits",
        ));
    }
    // A cancelled request may already have changed the adapter. Keep the last
    // acknowledged configuration but mark it uncertain until a full resend is
    // acknowledged. Never present a timed-out update as synchronized.
    store.groups.entry(group.clone()).or_default().synchronized = false;
    let body = session
        .call(group.command(), group.arguments(&next))
        .await?;
    let actual = body
        .get("breakpoints")
        .and_then(Value::as_array)
        .filter(|actual| actual.len() == next.len())
        .ok_or_else(|| {
            tool_err(
                "DAP_PROTOCOL",
                "adapter returned a mismatched breakpoint result count",
            )
        })?;
    if actual
        .iter()
        .any(|entry| !entry.is_object() || !entry["verified"].is_boolean())
    {
        return Err(tool_err(
            "DAP_PROTOCOL",
            "adapter breakpoint results lack verified flags",
        ));
    }
    let result = json!({
        "breakpoints": actual,
        "selected": selected.map(|index| actual[index].clone()),
        "count": next.len(),
        "synchronized": true
    });
    store.groups.insert(
        group,
        Set {
            requested: next,
            acknowledged: actual.clone(),
            synchronized: true,
        },
    );
    Ok(result)
}

pub(super) async fn inventory(session: &DapSession) -> Result<Value> {
    let owner = AgentCx::for_current_or_request();
    let store = OwnedMutexGuard::lock(Arc::clone(&session.breakpoints), owner.cx())
        .await
        .map_err(|_| tool_err("DAP_CANCELLED", "breakpoint inspection cancelled"))?;
    let groups: Vec<_> = store
        .groups
        .iter()
        .map(|(group, set)| {
            let mut value = json!({
                "command": group.command(), "requested": set.requested,
                "lastAcknowledged": set.acknowledged, "synchronized": set.synchronized
            });
            if let Group::Source(path) = group {
                value["file"] = json!(path);
            }
            value
        })
        .collect();
    Ok(
        json!({"groups": groups, "verification": "last set-request acknowledgement; pending breakpoints may bind later"}),
    )
}

/// Normalize aliases before using a source path as a replacement-set key.
pub(super) fn source_path(cwd: &Path, file: &str) -> Result<String> {
    if file.is_empty() || file.len() > 4096 || file.contains('\0') {
        return Err(tool_err(
            "DAP_USAGE",
            "file must be a nonempty path of at most 4096 bytes",
        ));
    }
    let path = cwd.join(file);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    // Resolve existing paths before lexical cleanup: link/../file follows the
    // link's target parent, not the parent of the link's directory entry.
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical
            .into_os_string()
            .into_string()
            .map_err(|_| tool_err("DAP_USAGE", "DAP source paths must be valid UTF-8"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    let canonical = std::fs::canonicalize(&normalized).unwrap_or(normalized);
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| tool_err("DAP_USAGE", "DAP source paths must be valid UTF-8"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceInput {
    pub file: String,
    pub line: u64,
    pub column: Option<u64>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

pub(super) fn source_spec(
    line: u64,
    column: Option<u64>,
    condition: Option<&str>,
    hit_condition: Option<&str>,
    log_message: Option<&str>,
) -> Result<Value> {
    if line == 0
        || line > i32::MAX as u64
        || column.is_some_and(|column| column == 0 || column > i32::MAX as u64)
    {
        return Err(tool_err(
            "DAP_USAGE",
            "line and column must be positive 32-bit integers",
        ));
    }
    let mut spec = json!({"line": line});
    if let Some(column) = column {
        spec["column"] = json!(column);
    }
    options(&mut spec, condition, hit_condition, log_message)?;
    Ok(spec)
}

pub(super) fn options(
    spec: &mut Value,
    condition: Option<&str>,
    hit_condition: Option<&str>,
    log_message: Option<&str>,
) -> Result<()> {
    for (field, value) in [
        ("condition", condition),
        ("hitCondition", hit_condition),
        ("logMessage", log_message),
    ] {
        if let Some(value) = value {
            if value.len() > 4096 || value.contains('\0') {
                return Err(tool_err(
                    "DAP_USAGE",
                    format!("{field} must be NUL-free and at most 4096 bytes"),
                ));
            }
            spec[field] = json!(value);
        }
    }
    Ok(())
}

pub(super) fn check_options(session: &DapSession, spec: &Value) -> Result<()> {
    for (field, capability) in [
        ("condition", "supportsConditionalBreakpoints"),
        ("hitCondition", "supportsHitConditionalBreakpoints"),
        ("logMessage", "supportsLogPoints"),
    ] {
        if spec.get(field).is_some() {
            session.require_capability(capability)?;
        }
    }
    Ok(())
}

pub(super) fn initial(cwd: &Path, inputs: &[SourceInput]) -> Result<BTreeMap<String, Vec<Value>>> {
    if inputs.len() > MAX_BREAKPOINTS {
        return Err(tool_err(
            "DAP_BREAKPOINT_LIMIT",
            "too many initial breakpoints",
        ));
    }
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for input in inputs {
        let path = source_path(cwd, &input.file)?;
        let spec = source_spec(
            input.line,
            input.column,
            input.condition.as_deref(),
            input.hit_condition.as_deref(),
            input.log_message.as_deref(),
        )?;
        let entries = groups.entry(path.clone()).or_default();
        if entries
            .iter()
            .any(|old| same_key(&Group::Source(path.clone()), old, &spec))
        {
            return Err(tool_err("DAP_USAGE", "duplicate initial source breakpoint"));
        }
        entries.push(spec);
    }
    if groups.len() > MAX_GROUPS {
        return Err(tool_err(
            "DAP_BREAKPOINT_LIMIT",
            "too many initial breakpoint sources",
        ));
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_changes_preserve_other_lines_and_columns() {
        let group = Group::Source("fixture.rs".into());
        let (set, _) = proposed(&group, &[], Change::Upsert(json!({"line": 10})));
        let (set, selected) = proposed(&group, &set, Change::Upsert(json!({"line": 20})));
        assert_eq!(selected, Some(1));
        let (set, _) = proposed(
            &group,
            &set,
            Change::Upsert(json!({"line": 10, "condition": "x > 2"})),
        );
        assert_eq!(
            set,
            vec![json!({"line":10,"condition":"x > 2"}), json!({"line":20})]
        );
        let (set, _) = proposed(&group, &set, Change::Remove(Some(json!({"line":10}))));
        assert_eq!(set, vec![json!({"line":20})]);
        let (set, _) = proposed(&group, &set, Change::Remove(None));
        assert!(set.is_empty());
    }

    #[test]
    fn all_breakpoint_families_have_stable_individual_keys() {
        for (group, first, second) in [
            (
                Group::Function,
                json!({"name":"first"}),
                json!({"name":"second"}),
            ),
            (
                Group::Instruction,
                json!({"instructionReference":"0x10"}),
                json!({"instructionReference":"0x20"}),
            ),
            (
                Group::Data,
                json!({"dataId":"opaque-A"}),
                json!({"dataId":"opaque-B"}),
            ),
        ] {
            let (set, _) = proposed(
                &group,
                std::slice::from_ref(&first),
                Change::Upsert(second.clone()),
            );
            assert_eq!(set.len(), 2);
            let (set, _) = proposed(&group, &set, Change::Remove(Some(first)));
            assert_eq!(set, vec![second]);
        }
    }

    #[test]
    fn invalid_source_positions_and_duplicate_aliases_are_rejected() {
        assert!(source_spec(0, None, None, None, None).is_err());
        assert!(source_spec(1, Some(0), None, None, None).is_err());
        let dir = tempfile::tempdir().unwrap();
        let inputs: Vec<SourceInput> = serde_json::from_value(json!([
            {"file":"missing.rs","line":3}, {"file":"./missing.rs","line":3}
        ]))
        .unwrap();
        assert!(initial(dir.path(), &inputs).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_path_resolves_symlinks_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(actual.join("nested")).unwrap();
        std::fs::write(workspace.join("file.rs"), "wrong source").unwrap();
        std::fs::write(actual.join("file.rs"), "correct source").unwrap();
        std::os::unix::fs::symlink(actual.join("nested"), workspace.join("link")).unwrap();
        assert_eq!(
            source_path(&workspace, "link/../file.rs").unwrap(),
            std::fs::canonicalize(actual.join("file.rs"))
                .unwrap()
                .display()
                .to_string()
        );
    }
}
