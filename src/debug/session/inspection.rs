//! Suspension-scoped frame/object handles. Adapter IDs may be reused after a
//! resume; model-facing IDs never are, including across sessions in this process.
//! Raw DAP access remains available through DapSession::call for SDK clients.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::sync::OwnedMutexGuard;
use serde_json::{Value, json};

use super::execution::Execution;
use super::{DapSession, State, tool_err};
use crate::agent_cx::AgentCx;
use crate::error::Result;

pub(super) const MAX_HANDLE: u64 = 9_007_199_254_740_991;
const MAX_HANDLES: usize = 8192;
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Frame,
    Variables,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Suspension {
    thread: u64,
    stop: u64,
    inspection: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Binding {
    kind: Kind,
    remote: u64,
    suspension: Suspension,
}

#[derive(Clone, Default)]
pub(super) struct Handles {
    entries: BTreeMap<u64, Binding>,
    reverse: BTreeMap<Binding, u64>,
}

impl Handles {
    fn retain(&mut self, mut keep: impl FnMut(&Binding) -> bool) {
        self.entries.retain(|_, binding| keep(binding));
        self.reverse
            .retain(|binding, id| self.entries.get(id) == Some(binding));
    }

    fn prune(&mut self, execution: &Execution, inspection: u64) {
        self.retain(|binding| {
            binding.suspension.inspection == inspection
                && execution.stamp(binding.suspension.thread) == Some(binding.suspension.stop)
        });
    }

    fn invalidate_variables(&mut self, thread: u64) {
        self.retain(|binding| {
            binding.kind != Kind::Variables || binding.suspension.thread != thread
        });
    }

    fn encode(&mut self, kind: Kind, remote: u64, suspension: Suspension) -> Result<u64> {
        if kind == Kind::Variables && remote == 0 {
            return Ok(0);
        }
        let binding = Binding {
            kind,
            remote,
            suspension,
        };
        if let Some(id) = self.reverse.get(&binding) {
            return Ok(*id);
        }
        if self.entries.len() >= MAX_HANDLES {
            return Err(tool_err(
                "DAP_HANDLE_LIMIT",
                "debug suspension exceeds 8192 live frame/object handles; request smaller pages",
            ));
        }
        let id = allocate(&NEXT_HANDLE)?;
        self.entries.insert(id, binding);
        self.reverse.insert(binding, id);
        Ok(id)
    }

    fn resolve(&self, kind: Kind, id: u64) -> Result<Binding> {
        self.entries.get(&id).copied().filter(|binding| binding.kind == kind)
            .ok_or_else(|| tool_err("DAP_STALE_REFERENCE", "unknown, expired or wrong-kind debugger handle; refresh stack_trace, scopes or evaluate"))
    }
}

fn allocate(counter: &AtomicU64) -> Result<u64> {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |id| {
            if id <= MAX_HANDLE { Some(id + 1) } else { None }
        })
        .map_err(|_| {
            tool_err(
                "DAP_HANDLE_LIMIT",
                "debugger handle IDs exhausted; refusing to reuse an old ID",
            )
        })
}

fn suspension(state: &State, thread: u64) -> Result<Suspension> {
    state.check()?;
    state.execution.require(Some(thread))?;
    let stop = state
        .execution
        .stamp(thread)
        .ok_or_else(|| tool_err("DAP_STATE_RUNNING", "selected thread is no longer stopped"))?;
    Ok(Suspension {
        thread,
        stop,
        inspection: state.inspection_revision,
    })
}

fn remote_id(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|id| *id <= 2_147_483_647)
        .ok_or_else(|| {
            tool_err(
                "DAP_PROTOCOL",
                "adapter returned an invalid frame/object reference",
            )
        })
}

fn map_variable(
    handles: &mut Handles,
    value: &mut Value,
    at: Suspension,
    required: bool,
) -> Result<()> {
    match value.get("variablesReference") {
        Some(reference) => {
            let id = handles.encode(Kind::Variables, remote_id(reference)?, at)?;
            value["variablesReference"] = json!(id);
        }
        None if required => {
            return Err(tool_err(
                "DAP_PROTOCOL",
                "adapter omitted variablesReference",
            ));
        }
        None => {}
    }
    // These location handles have a suspension lifetime but no typed resolver
    // in Pi yet. Do not expose raw numeric references as usable local handles.
    if let Some(object) = value.as_object_mut() {
        object.remove("valueLocationReference");
        object.remove("declarationLocationReference");
    }
    Ok(())
}

fn rewrite_reply(
    handles: &mut Handles,
    command: &str,
    mut body: Value,
    at: Suspension,
) -> Result<Value> {
    if !body.is_object() {
        return Err(tool_err(
            "DAP_PROTOCOL",
            "inspection response must be an object",
        ));
    }
    match command {
        "stackTrace" => {
            let frames = body["stackFrames"]
                .as_array_mut()
                .filter(|frames| frames.len() <= MAX_HANDLES)
                .ok_or_else(|| {
                    tool_err("DAP_PROTOCOL", "invalid or oversized stack frame response")
                })?;
            for frame in frames {
                frame["id"] = json!(handles.encode(Kind::Frame, remote_id(&frame["id"])?, at)?);
            }
        }
        "scopes" | "variables" => {
            let key = if command == "scopes" {
                "scopes"
            } else {
                "variables"
            };
            let entries = body[key]
                .as_array_mut()
                .filter(|entries| entries.len() <= MAX_HANDLES)
                .ok_or_else(|| {
                    tool_err(
                        "DAP_PROTOCOL",
                        "invalid or oversized scope/variable response",
                    )
                })?;
            for entry in entries {
                map_variable(handles, entry, at, true)?;
            }
        }
        "evaluate" | "setVariable" | "setExpression" => {
            let field = if command == "evaluate" {
                "result"
            } else {
                "value"
            };
            if !body[field].is_string() {
                return Err(tool_err(
                    "DAP_PROTOCOL",
                    format!("adapter omitted {field} in inspection response"),
                ));
            }
            map_variable(handles, &mut body, at, command == "evaluate")?;
        }
        "exceptionInfo" => {
            if !body["exceptionId"].is_string()
                || !matches!(
                    body["breakMode"].as_str(),
                    Some("never" | "always" | "unhandled" | "userUnhandled")
                )
            {
                return Err(tool_err(
                    "DAP_PROTOCOL",
                    "adapter returned incomplete exception information",
                ));
            }
        }
        "dataBreakpointInfo" => {}
        _ => {
            return Err(tool_err(
                "DAP_USAGE",
                "unsupported managed inspection request",
            ));
        }
    }
    Ok(body)
}

/// Only the typed inspection path rewrites handles. Each call rechecks the
/// owning thread before dispatch and after the response. No std mutex spans an
/// await; the owned async guard serializes handle allocation and mutation.
pub(super) async fn call(
    session: &DapSession,
    command: &str,
    mut arguments: Value,
) -> Result<Value> {
    match command {
        "setVariable" => session.require_capability("supportsSetVariable")?,
        "setExpression" => session.require_capability("supportsSetExpression")?,
        "exceptionInfo" => session.require_capability("supportsExceptionInfoRequest")?,
        _ => {}
    }
    let owner = AgentCx::for_current_or_request();
    let mut handles = OwnedMutexGuard::lock(Arc::clone(&session.inspection), owner.cx())
        .await
        .map_err(|_| tool_err("DAP_CANCELLED", "debug inspection cancelled while queued"))?;
    session.pump_events();
    let before = {
        let mut bound = None;
        for (field, kind) in [
            ("frameId", Kind::Frame),
            ("variablesReference", Kind::Variables),
        ] {
            if let Some(value) = arguments.get(field) {
                let local = value.as_u64().ok_or_else(|| {
                    tool_err("DAP_USAGE", "frame/object handle must be an integer")
                })?;
                let binding = handles.resolve(kind, local)?;
                if bound.is_some_and(|at| at != binding.suspension) {
                    return Err(tool_err(
                        "DAP_STALE_REFERENCE",
                        "cannot combine handles from different suspended threads",
                    ));
                }
                bound = Some(binding.suspension);
                arguments[field] = json!(binding.remote);
            }
        }
        let selected = arguments.get("threadId").and_then(Value::as_u64);
        let state = DapSession::lock(&session.state);
        state.check()?;
        handles.prune(&state.execution, state.inspection_revision);
        let res = if let Some(at) = bound {
            if selected.is_some_and(|thread| thread != at.thread) {
                return Err(tool_err(
                    "DAP_USAGE",
                    "threadId does not own the supplied frame/object handle",
                ));
            }
            match suspension(&state, at.thread) {
                Ok(current) if current == at => at,
                _ => {
                    return Err(tool_err(
                        "DAP_STALE_REFERENCE",
                        "debugger handle outlived its suspension",
                    ));
                }
            }
        } else {
            let thread = state.execution.require(selected)?;
            if thread == 0 {
                return Err(tool_err(
                    "DAP_NO_THREADS",
                    "query threads and select a stopped thread before inspection",
                ));
            }
            suspension(&state, thread)?
        };
        drop(state);
        res
    };
    // threadId on object/frame actions is Pi's ownership precondition, not a
    // field defined by DAP for scopes, variables, evaluate or assignments.
    if !matches!(command, "stackTrace" | "exceptionInfo")
        && let Some(object) = arguments.as_object_mut()
    {
        object.remove("threadId");
    }
    let mutating = matches!(command, "setVariable" | "setExpression");
    if mutating {
        // Revoke before awaiting: timeout or future-drop cannot leave old
        // object handles usable after a remotely accepted assignment.
        handles.invalidate_variables(before.thread);
    }
    let body = session.call(command, arguments).await?;
    session.pump_events();
    let after = {
        let state = DapSession::lock(&session.state);
        handles.prune(&state.execution, state.inspection_revision);
        let res = suspension(&state, before.thread)?;
        drop(state);
        res
    };
    if before.stop != after.stop || (!mutating && before.inspection != after.inspection) {
        return Err(tool_err(
            "DAP_STALE_REFERENCE",
            "debuggee changed suspension or invalidated inspection during the request; refresh the stack",
        ));
    }
    // Validate and rewrite transactionally. A malformed response cannot leave
    // partially exposed handle state or consume the session's live-handle cap.
    let mut candidate = (*handles).clone();
    let body = rewrite_reply(&mut candidate, command, body, after)?;
    *handles = candidate;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(stop: u64) -> Suspension {
        Suspension {
            thread: 7,
            stop,
            inspection: 0,
        }
    }

    #[test]
    fn frame_and_variable_handles_are_typed_and_zero_is_only_a_leaf() {
        let mut handles = Handles::default();
        let frame = handles.encode(Kind::Frame, 0, at(1)).unwrap();
        let object = handles.encode(Kind::Variables, 66, at(1)).unwrap();
        assert_ne!(frame, object);
        assert_eq!(handles.resolve(Kind::Frame, frame).unwrap().remote, 0);
        assert!(handles.resolve(Kind::Frame, object).is_err());
        assert!(handles.resolve(Kind::Variables, 0).is_err());
        assert_eq!(handles.encode(Kind::Variables, 0, at(1)).unwrap(), 0);
        assert_eq!(handles.encode(Kind::Variables, 66, at(1)).unwrap(), object);
    }

    #[test]
    fn recycled_adapter_ids_never_reuse_a_model_facing_handle() {
        let mut handles = Handles::default();
        let old = handles.encode(Kind::Variables, 66, at(1)).unwrap();
        handles.retain(|_| false);
        let new = handles.encode(Kind::Variables, 66, at(2)).unwrap();
        assert_ne!(old, new);
        assert!(handles.resolve(Kind::Variables, old).is_err());
        let other_session = Handles::default()
            .encode(Kind::Variables, 66, at(2))
            .unwrap();
        assert_ne!(other_session, new);
    }

    #[test]
    fn assignments_invalidate_objects_but_not_their_stack_frames() {
        let mut handles = Handles::default();
        let frame = handles.encode(Kind::Frame, 21, at(1)).unwrap();
        let object = handles.encode(Kind::Variables, 66, at(1)).unwrap();
        let peer = handles
            .encode(Kind::Variables, 77, Suspension { thread: 8, ..at(1) })
            .unwrap();
        handles.invalidate_variables(7);
        assert!(handles.resolve(Kind::Frame, frame).is_ok());
        assert!(handles.resolve(Kind::Variables, object).is_err());
        assert!(handles.resolve(Kind::Variables, peer).is_ok());
    }

    #[test]
    fn reply_rewriting_preserves_values_and_rejects_missing_references() {
        let mut handles = Handles::default();
        let value = rewrite_reply(&mut handles, "evaluate", json!({"result":"object","variablesReference":66,"namedVariables":2,"memoryReference":"0x100"}), at(1)).unwrap();
        let local = value["variablesReference"].as_u64().unwrap();
        assert_eq!(handles.resolve(Kind::Variables, local).unwrap().remote, 66);
        assert_eq!(value["namedVariables"], 2);
        assert_eq!(value["memoryReference"], "0x100");
        assert!(
            rewrite_reply(
                &mut handles,
                "variables",
                json!({"variables":[{"name":"x","value":"1"}]}),
                at(1)
            )
            .is_err()
        );
        assert!(
            rewrite_reply(
                &mut handles,
                "exceptionInfo",
                json!({"exceptionId":"Error"}),
                at(1)
            )
            .is_err()
        );
    }

    #[test]
    fn handle_ids_stop_before_json_integer_precision_is_lost() {
        let counter = AtomicU64::new(MAX_HANDLE);
        assert_eq!(allocate(&counter).unwrap(), MAX_HANDLE);
        assert!(allocate(&counter).is_err());
        assert!(allocate(&AtomicU64::new(u64::MAX)).is_err());
    }
}
