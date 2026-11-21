//! Bounded, invocation-owned native tool batches. The existing Agent policy
//! path remains authoritative for every call; scheduling metadata is not an
//! execution capability, and parallel dispatch rechecks the actual tool.

use futures::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Invocation, MAX_TOOL_CALLS};
use crate::agent_cx::AgentCx;
use crate::eval::{EvalTool, bridge_result, session::CellSession};
use crate::model::ToolCall;
use crate::tools::{Tool, ToolInvocationHost, ToolOutput};

pub(super) const BRIDGE_NAME: &str = "__pi_tool_batch";
const MAX_ITEMS: usize = 16;
const MAX_PARALLEL: usize = 8;
const REQUEST_LIMIT: usize = 256 * 1024;
const REPLY_PREFIX: &str = "{\"schema\":\"pi.eval.tool_batch.v1\",\"items\":[";
const REPLY_SUFFIX: &str = "]}";
const RESULT_LIMIT_ERROR: &str = "EVAL_BRIDGE_LIMIT: batch result exceeds 4 MiB; calls may have completed, inspect this cell's tool receipts before retrying";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    calls: Vec<InputCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputCall {
    name: String,
    args: Value,
}

fn parse(input: Value) -> Result<Request, String> {
    bridge_result::check_value(&input)?;
    bridge_result::encode_limited(
        &input,
        REQUEST_LIMIT,
        "EVAL_BRIDGE_LIMIT: batch input exceeds 256 KiB",
    )?;
    let request: Request = serde_json::from_value(input)
        .map_err(|_| "EVAL_TOOL_BATCH_INPUT: expected only calls with name and object args")?;
    if request.calls.is_empty() || request.calls.len() > MAX_ITEMS {
        return Err("EVAL_TOOL_BATCH_INPUT: expected 1 through 16 calls".into());
    }
    for call in &request.calls {
        if call.name.is_empty()
            || call.name.len() > 128
            || call.name.chars().any(char::is_control)
            || !call.args.is_object()
        {
            return Err("EVAL_TOOL_BATCH_INPUT: expected a bounded tool name and object args".into());
        }
        if matches!(call.name.as_str(), "eval" | "xdev") || call.name.starts_with("__pi_") {
            return Err("EVAL_BRIDGE_RECURSION: batches cannot call eval or dispatch wrappers".into());
        }
    }
    Ok(request)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item<'a, T> {
    index: usize,
    name: &'a str,
    tool_call_id: &'a str,
    #[serde(flatten)]
    outcome: T,
}

#[derive(Serialize)]
struct Returned<T> {
    result: T,
}

#[derive(Serialize)]
struct Failed<'a> {
    error: &'a str,
}

fn encode_failure(index: usize, call: &ToolCall, error: &str) -> Result<String, String> {
    bridge_result::encode_limited(
        &Item {
            index,
            name: &call.name,
            tool_call_id: &call.id,
            outcome: Failed { error },
        },
        bridge_result::STRUCTURED_LIMIT,
        "EVAL_BRIDGE_LIMIT: batch item error exceeds 4 MiB",
    )
}

fn encode_outcome(
    index: usize,
    call: &ToolCall,
    outcome: &Result<ToolOutput, String>,
) -> Result<String, String> {
    let encoded = match outcome {
        Ok(output) => bridge_result::structured_envelope(output).and_then(|result| {
            bridge_result::encode_limited(
                &Item {
                    index,
                    name: &call.name,
                    tool_call_id: &call.id,
                    outcome: Returned { result },
                },
                bridge_result::STRUCTURED_LIMIT,
                "EVAL_BRIDGE_LIMIT: batch item result exceeds 4 MiB",
            )
        }),
        Err(error) => encode_failure(index, call, error),
    };
    // Invalid or individually oversized outputs become explicit item failures.
    // Neither the guest nor the receipt sees a fabricated successful result.
    encoded.or_else(|error| encode_failure(index, call, &error))
}

/// Keep at most one aggregate budget of pending item payloads, regardless of
/// completion order. Final encoding also allocates the returned string.
/// Once the total overflows, discard payloads but continue settling admitted
/// work. Returning a successful prefix or stopping midway could invite replay
/// of side effects that have already happened.
struct Reply {
    items: Vec<Option<String>>,
    bytes: usize,
    oversized: bool,
}

impl Reply {
    fn new(count: usize) -> Self {
        Self {
            items: vec![None; count],
            bytes: REPLY_PREFIX.len() + REPLY_SUFFIX.len() + count.saturating_sub(1),
            oversized: false,
        }
    }

    fn insert(&mut self, index: usize, item: String) {
        if self.oversized {
            return;
        }
        self.bytes = self.bytes.saturating_add(item.len());
        if self.bytes > bridge_result::STRUCTURED_LIMIT {
            self.oversized = true;
            for item in &mut self.items {
                *item = None;
            }
        } else {
            self.items[index] = Some(item);
        }
    }

    fn finish(self) -> Result<String, String> {
        if self.oversized {
            return Err(RESULT_LIMIT_ERROR.into());
        }
        let mut reply = String::with_capacity(self.bytes);
        reply.push_str(REPLY_PREFIX);
        for (index, item) in self.items.into_iter().enumerate() {
            if index > 0 {
                reply.push(',');
            }
            reply.push_str(&item.ok_or("EVAL_TOOL_BATCH_PROTOCOL: item did not settle")?);
        }
        reply.push_str(REPLY_SUFFIX);
        Ok(reply)
    }
}

pub(super) async fn execute_call(
    host: Option<&dyn ToolInvocationHost>,
    native: Option<&dyn Tool>,
    call: ToolCall,
    expected_session: &str,
    session: &CellSession,
    parallel: bool,
) -> Result<ToolOutput, String> {
    let owner = AgentCx::for_current_or_request();
    owner
        .checkpoint()
        .map_err(|_| "EVAL_CANCELLED: nested tool stopped")?;
    session.check_tool().await.map_err(|err| err.to_string())?;
    let result = match host {
        Some(host) if parallel => host.call_tool_parallel(call, expected_session).await,
        Some(host) => host.call_tool(call, expected_session).await,
        None => {
            let native = native.ok_or("EVAL_BRIDGE_DENIED: no standalone tool authority")?;
            native.execute(&call.id, call.arguments, None).await
        }
    };
    owner
        .checkpoint()
        .map_err(|_| "EVAL_CANCELLED: nested tool stopped")?;
    session.check_tool().await.map_err(|err| err.to_string())?;
    result.map_err(|err| err.to_string())
}

impl Invocation<'_> {
    pub(super) async fn call_tool_batch(
        &mut self,
        tool: &EvalTool,
        input: Value,
    ) -> Result<String, String> {
        let request = parse(input)?;
        if request.calls.len() > MAX_TOOL_CALLS.saturating_sub(self.tool_receipts.len()) {
            return Err("EVAL_BRIDGE_LIMIT: batch exceeds the remaining 64 tool calls per cell".into());
        }
        let expected_session = self
            .host_session
            .clone()
            .ok_or("EVAL_SESSION_UNAVAILABLE: native cell owner was not captured")?;
        let session = self.cell_session(tool)?;
        let host = self.host;
        // Pin the same concrete local handles whose effects we classify. The
        // whitelist is shared with scalar calls and validated before admission.
        let native = if host.is_none() {
            Some(
                request
                    .calls
                    .iter()
                    .map(|call| tool.standalone_bridge_tool(&call.name))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        } else {
            None
        };
        let calls = request
            .calls
            .into_iter()
            .map(|call| self.reserve_tool_call(call.name, call.args))
            .collect::<Result<Vec<_>, _>>()?;
        let effects = calls
            .iter()
            .enumerate()
            .map(|(index, (_, call))| {
                host.map_or_else(
                    || {
                        native
                            .as_ref()
                            .map_or_else(crate::tools::ToolEffects::write, |tools| {
                                tools[index].effects_for_input(&call.arguments)
                            })
                    },
                    |host| host.tool_effects(call),
                )
            })
            .collect::<Vec<_>>();
        let parallelism = host
            .map_or(MAX_PARALLEL, |host| host.tool_parallelism())
            .clamp(1, MAX_PARALLEL);
        let mut reply = Reply::new(calls.len());
        for batch in crate::agent::plan_tool_effect_batches(&effects) {
            let parallel = parallelism > 1 && batch.end - batch.start > 1;
            let operations = (batch.start..batch.end).map(|index| {
                let call = calls[index].1.clone();
                let native = native.as_ref().map(|tools| tools[index].as_ref());
                let expected_session = expected_session.as_str();
                let session = &session;
                async move {
                    (
                        index,
                        execute_call(host, native, call, expected_session, session, parallel).await,
                    )
                }
            });
            let mut pending = stream::iter(operations).buffer_unordered(parallelism);
            while let Some((index, outcome)) = pending.next().await {
                // Update before projection. A later timeout must not discard
                // already observed success/failure merely because a peer waits.
                self.finish_tool_receipt(calls[index].0, &outcome)?;
                if !reply.oversized {
                    reply.insert(index, encode_outcome(index, &calls[index].1, &outcome)?);
                }
            }
        }
        reply.finish()
    }
}

#[cfg(test)]
mod tests;
