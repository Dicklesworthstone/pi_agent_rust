//! Dependency-ordered native delegation, not a second child runtime.
//!
//! Each topological wave uses ChildRunner's captured parallel baseline and
//! ordered acceptance. The next wave sees accepted writeback, never provisional
//! child edits. Failed dependencies skip their descendants; independent work
//! continues under the same request deadline and parent model snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    AgentDefinition, AgentScope, ChildRunner, DEFAULT_CONCURRENCY, Deadline, Error, Result,
    SubagentResult, SubagentStatus, SubagentTask, SubagentTool, UpdateCallback,
    compile_output_schema, render_results, workspace,
};
use crate::agent_cx::AgentCx;
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;

const MAX_NODES: usize = 32;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_TASK_BYTES: usize = 64 * 1024;

fn invalid(message: &'static str) -> Error {
    Error::tool("subagent", format!("PI_WORKFLOW_INVALID: {message}"))
}

/// Count before copying a request or a typed dependency result. JSON escaping
/// counts too. The sink never creates a second serialized copy of the payload.
fn encoded_size(value: &impl Serialize, limit: usize) -> Option<usize> {
    struct Counter { bytes: usize, limit: usize }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(io::Error::other("workflow byte limit"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.bytes)
}

fn identifier(value: &Value) -> Result<&str> {
    value.as_str().filter(|id| {
        !id.is_empty() && id.len() <= 64
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }).ok_or_else(|| invalid("node IDs must be 1-64 ASCII letters, digits, underscores or hyphens"))
}

struct Node {
    id: String,
    needs: Vec<usize>,
    call: SubagentTask,
}

pub(super) struct Plan {
    nodes: Vec<Node>,
    waves: Vec<Vec<usize>>,
    concurrency: usize,
    timeout_seconds: Option<u64>,
    scope: AgentScope,
}

impl Plan {
    /// Every graph edge, task shape and workspace option is checked before
    /// discovery, snapshot I/O or launch. Forward references are permitted;
    /// cycles, duplicate edges and mixed request modes are not.
    pub(super) fn parse(input: &Value) -> Result<Option<Self>> {
        let Some(raw) = input.get("workflow") else { return Ok(None); };
        if encoded_size(input, MAX_REQUEST_BYTES).is_none() {
            return Err(invalid("workflow request exceeds 256 KiB"));
        }
        let fields = input.as_object().ok_or_else(|| invalid("expected an object"))?;
        if fields.keys().any(|key| !matches!(key.as_str(),
            "workflow" | "concurrency" | "timeoutSeconds" | "scope"
        )) {
            return Err(invalid("workflow cannot be combined with single, tasks, chain or other top-level task controls"));
        }
        let entries = raw.as_array().filter(|entries| (1..=MAX_NODES).contains(&entries.len()))
            .ok_or_else(|| invalid("workflow requires 1-32 nodes"))?;
        let concurrency = match input.get("concurrency") {
            None => DEFAULT_CONCURRENCY,
            Some(value) => value.as_u64()
                .filter(|value| (1..=super::MAX_PARALLEL_TASKS as u64).contains(value))
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| invalid("concurrency must be an integer from 1 to 8"))?,
        };
        let timeout_seconds = input.get("timeoutSeconds").map(|value| {
            value.as_u64().filter(|value| (1..=super::deadline::MAX_TIMEOUT_SECS).contains(value))
                .ok_or_else(|| invalid("timeoutSeconds must be an integer from 1 to 86400"))
        }).transpose()?;
        let scope = input.get("scope").map(|value| serde_json::from_value(value.clone())
            .map_err(|_| invalid("scope must be both, user or project")))
            .transpose()?.unwrap_or(AgentScope::Both);
        let mut ids = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            let id = identifier(&entry["id"])?;
            if ids.insert(id.to_owned(), index).is_some() {
                return Err(invalid("workflow node IDs must be unique"));
            }
        }
        let mut nodes = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let fields = entry.as_object().ok_or_else(|| invalid("each node must be an object"))?;
            if fields.keys().any(|key| !matches!(key.as_str(), "id" | "needs" | "call")) {
                return Err(invalid("node accepts only id, needs and call"));
            }
            let needs = match fields.get("needs") {
                None => Vec::new(),
                Some(value) => {
                    let array = value.as_array().filter(|array| array.len() < MAX_NODES)
                        .ok_or_else(|| invalid("needs must be an array of at most 31 distinct node IDs"))?;
                    let mut seen = BTreeSet::new();
                    let mut needs = Vec::with_capacity(array.len());
                    for id in array {
                        let dependency = ids.get(identifier(id)?).copied()
                            .ok_or_else(|| invalid("needs names an unknown node"))?;
                        if dependency == index || !seen.insert(dependency) {
                            return Err(invalid("self dependencies and duplicate edges are not allowed"));
                        }
                        needs.push(dependency);
                    }
                    needs
                }
            };
            let call = parse_call(&entry["call"])?;
            nodes.push(Node { id: identifier(&entry["id"])?.to_owned(), needs, call });
        }
        let waves = layers(&nodes)?;
        Ok(Some(Self { nodes, waves, concurrency, timeout_seconds, scope }))
    }

    fn validate_definitions(&self, agents: &BTreeMap<String, AgentDefinition>) -> Result<()> {
        for node in &self.nodes {
            let agent = agents.get(&node.call.agent)
                .ok_or_else(|| invalid("workflow references an undiscovered agent; no node was launched"))?;
            if let Some(schema) = node.call.output_schema.as_ref().or(agent.output_schema.as_ref()) {
                compile_output_schema(schema)
                    .map_err(|_| invalid("a workflow output schema is invalid; no node was launched"))?;
            }
        }
        Ok(())
    }

    pub(super) async fn execute(self, tool: &SubagentTool, update: Option<UpdateCallback>) -> Result<ToolOutput> {
        let deadline = Deadline::for_request(tool.timeout, self.timeout_seconds)?;
        let parent_model = tool.capture_parent_model();
        let owner = AgentCx::for_current_or_request();
        check_authority(&owner)?;
        deadline.check().map_err(|message| Error::tool("subagent", message))?;
        let agents = tool.discover(self.scope)?;
        self.validate_definitions(&agents)?;
        let runner = ChildRunner::new(
            tool.cwd.clone(), tool.global_dir.clone(), tool.child_binary.clone(),
            tool.role_model_spec.clone(), crate::agent_hub::ChildKind::Subagent, deadline,
        ).with_parent_model(parent_model);
        let mut records: Vec<_> = self.nodes.iter().map(|node| Record::pending(node, &self.nodes)).collect();
        let mut waves_run = 0;
        let mut stop_reason = None;
        for (wave, indices) in self.waves.iter().enumerate() {
            if owner.checkpoint().is_err() {
                stop_reason = Some("cancelled");
                break;
            }
            if deadline.check().is_err() {
                stop_reason = Some("deadline_exhausted");
                break;
            }
            let mut admitted = Vec::new();
            for &index in indices {
                let blocked: Vec<_> = self.nodes[index].needs.iter()
                    .filter(|&&dependency| records[dependency].state != State::Accepted)
                    .map(|&dependency| self.nodes[dependency].id.clone()).collect();
                if blocked.is_empty() {
                    admitted.push(index);
                } else {
                    records[index].skip("dependency_failed", blocked);
                }
            }
            if admitted.is_empty() { continue; }
            let tasks = admitted.iter().map(|&index| self.nodes[index].call.clone()).collect();
            // This barrier is intentional. All peers use one captured baseline
            // per root; accepted writes settle in authored order before any
            // downstream node can observe them. No alternate spawn/apply path.
            let results = runner.run_parallel(&agents, tasks, self.concurrency, update.clone()).await;
            if results.len() != admitted.len() {
                return Err(Error::tool("subagent", "PI_WORKFLOW_INTERNAL: native runner returned an incomplete wave; effects may have occurred"));
            }
            waves_run += 1;
            for (index, result) in admitted.into_iter().zip(results) {
                records[index].settle(result, wave);
            }
        }
        for record in &mut records {
            if record.state == State::Pending {
                record.skip(stop_reason.unwrap_or("not_scheduled"), Vec::new());
            }
        }
        Ok(report(records, waves_run, stop_reason))
    }
}

fn parse_call(value: &Value) -> Result<SubagentTask> {
    let fields = value.as_object().ok_or_else(|| invalid("node call must be a native task object"))?;
    if fields.keys().any(|key| !matches!(key.as_str(),
        "agent" | "task" | "cwd" | "isolation" | "isoApply" | "outputSchema" | "schemaMode"
    )) {
        return Err(invalid("node call contains unsupported native task fields"));
    }
    for key in ["agent", "task"] {
        if fields.get(key).and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty() || text.len() > MAX_TASK_BYTES || text.contains('\0'))
        {
            return Err(invalid("agent and task must be nonempty strings, at most 64 KiB, without NUL bytes"));
        }
    }
    workspace::validate_request(value)?;
    if fields.get("outputSchema").is_some_and(|schema| !schema.is_object() && !schema.is_boolean()) {
        return Err(invalid("outputSchema must be a JSON Schema object or boolean, not null"));
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid("invalid native task arguments"))
}

/// Kahn waves in authored order; never recurse through a model-provided graph.
fn layers(nodes: &[Node]) -> Result<Vec<Vec<usize>>> {
    let mut seen = vec![false; nodes.len()];
    let mut count = 0;
    let mut waves = Vec::new();
    while count < nodes.len() {
        let ready: Vec<_> = nodes.iter().enumerate()
            .filter(|(index, node)| !seen[*index] && node.needs.iter().all(|&dependency| seen[dependency]))
            .map(|(index, _)| index).collect();
        if ready.is_empty() { return Err(invalid("workflow contains a dependency cycle")); }
        for &index in &ready { seen[index] = true; }
        count += ready.len();
        waves.push(ready);
    }
    Ok(waves)
}

fn check_authority(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| Error::tool("subagent", "PI_WORKFLOW_CANCELLED: parent is cancelled"))?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(Error::tool("subagent", "PI_SUBAGENT_PERMISSION: workflow requires I/O, spawn and timer capabilities"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State { Pending, Accepted, Rejected, Skipped }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    needs: Vec<String>,
    state: State,
    wave: Option<usize>,
    skip_reason: Option<&'static str>,
    blocked_by: Vec<String>,
    hub_id: Option<String>,
    native_is_error: Option<bool>,
    result: Option<SubagentResult>,
}

impl Record {
    fn pending(node: &Node, nodes: &[Node]) -> Self {
        Self {
            id: node.id.clone(), needs: node.needs.iter().map(|&index| nodes[index].id.clone()).collect(),
            state: State::Pending, wave: None, skip_reason: None, blocked_by: Vec::new(),
            hub_id: None, native_is_error: None, result: None,
        }
    }

    fn skip(&mut self, reason: &'static str, blocked: Vec<String>) {
        self.state = State::Skipped;
        self.skip_reason = Some(reason);
        self.blocked_by = blocked;
    }

    fn settle(&mut self, result: SubagentResult, wave: usize) {
        // A permissive schema warning is not trustworthy predecessor evidence.
        // Native acceptance still controls actual worktree disposition.
        self.state = if !result.is_error && matches!(result.status, SubagentStatus::Completed)
            && result.schema_valid != Some(false)
        { State::Accepted } else { State::Rejected };
        self.wave = Some(wave);
        self.hub_id = result.hub_id.clone();
        self.native_is_error = Some(result.is_error);
        self.result = Some(result);
    }
}

fn report(records: Vec<Record>, waves: usize, stop: Option<&'static str>) -> ToolOutput {
    let accepted = records.iter().filter(|record| record.state == State::Accepted).count();
    let rejected = records.iter().filter(|record| record.state == State::Rejected).count();
    let skipped = records.iter().filter(|record| record.state == State::Skipped).count();
    let mut content = format!(
        "Subagent workflow: {accepted} accepted, {rejected} rejected, {skipped} skipped. Completed effects are not rolled back; nodes were not automatically replayed."
    );
    for record in &records {
        content.push_str(&format!("\n\n# Workflow node {}\n", record.id));
        if let Some(result) = &record.result {
            content.push_str(&render_results(std::slice::from_ref(result)));
        } else {
            content.push_str(&format!("Not run: {}. Blocked by: {}.",
                record.skip_reason.unwrap_or("not_scheduled"), record.blocked_by.join(", ")));
        }
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(content))],
        details: Some(json!({
            "schema":"pi.subagent.workflow.v1", "mode":"workflow",
            "sessionIsolation":"ephemeral_no_session", "totalNodes":records.len(),
            "acceptedNodes":accepted, "rejectedNodes":rejected, "skippedNodes":skipped,
            "wavesExecuted":waves, "stopReason":stop, "nodes":records,
        })),
        is_error: rejected != 0 || skipped != 0 || stop.is_some(),
    }
}

pub(super) fn schema(mut schema: Value) -> Value {
    // Keep existing single/parallel/chain schemas untouched. A workflow call
    // admits JSON Schema booleans as well as objects, just like the runner.
    let mut task_schema = schema["definitions"]["task"].clone();
    task_schema["additionalProperties"] = json!(false);
    task_schema["properties"]["outputSchema"]["type"] = json!(["object", "boolean"]);
    schema["definitions"]["workflowTask"] = task_schema;
    schema["properties"]["workflow"] = json!({
        "type":"array", "minItems":1, "maxItems":MAX_NODES,
        "description":"Dependency graph, mutually exclusive with agent/task, tasks or chain. Each node has id, a native call and optional needs IDs. Forward references are allowed; cycles fail before launch. Ready nodes run in deterministic waves with concurrency 1-8. Native schema and worktree acceptance finish before dependants run. Failed or schema-invalid dependencies skip descendants; independent branches continue. One original timeout and parent-model snapshot cover the entire graph; no replay or graph-wide rollback. Results are in authored node order with hub IDs and explicit skipped reasons.",
        "items":{"type":"object", "additionalProperties":false, "required":["id","call"],
            "properties":{
                "id":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"},
                "needs":{"type":"array","maxItems":31,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}},
                "call":{"$ref":"#/definitions/workflowTask"}
            }}
    });
    schema
}

#[cfg(test)]
mod tests;
