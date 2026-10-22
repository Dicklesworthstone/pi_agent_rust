//! Read newly settled batch judgments without consuming the native job.
//!
//! Cursors are explicit acknowledgments, not authority. Only current host tool
//! results leave this operation. Registry observations are readiness hints;
//! after waiting, a second ordinary host lookup rechecks policy and projects
//! the returned evidence. Nothing retains a host beyond the invoking cell.

use super::{AgentCx, Invocation};
use crate::eval::EvalTool;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::time::Duration;

pub(super) const BRIDGE_NAME: &str = "__pi_judge_batch_drain";
const CURSOR_SCHEMA: &str = "pi.eval.batch-cursor.v1";
const INPUT: &str = "EVAL_BATCH_INPUT: expected jobId, optional cursor and timeoutMs from 0 through 600000";
const PROTOCOL: &str = "EVAL_BATCH_PROTOCOL: invalid or incompatible native batch observation";
const CURSOR: &str = "EVAL_BATCH_CURSOR: cursor does not match this batch's retained evidence";
const HOST: &str = "EVAL_BATCH_HOST: current jobs lookup was refused";
const POLL_INTERVAL: Duration = Duration::from_millis(100);

type Result<T> = std::result::Result<T, String>;

struct Request {
    id: String,
    seen: u16,
    manifest: Option<String>,
    timeout: Duration,
}

impl Request {
    fn parse(input: &Value) -> Result<Self> {
        let fields = input.as_object().ok_or(INPUT)?;
        if fields.keys().any(|key| !matches!(key.as_str(), "jobId" | "cursor" | "timeoutMs")) {
            return Err(INPUT.into());
        }
        let id = fields.get("jobId").and_then(Value::as_str).filter(|id| {
            !id.is_empty() && id.len() <= 128
                && !id.chars().any(|c| c.is_whitespace() || c.is_control())
        }).ok_or(INPUT)?;
        let timeout = match fields.get("timeoutMs") {
            None => 30_000,
            Some(value) => value.as_u64().filter(|value| *value <= 600_000).ok_or(INPUT)?,
        };
        let mut seen = 0_u16;
        let mut manifest = None;
        if let Some(cursor) = fields.get("cursor").filter(|cursor| !cursor.is_null()) {
            let cursor = cursor.as_object().filter(|cursor| cursor.len() == 4).ok_or(CURSOR)?;
            if cursor.get("schema").and_then(Value::as_str) != Some(CURSOR_SCHEMA)
                || cursor.get("jobId").and_then(Value::as_str) != Some(id)
            { return Err(CURSOR.into()); }
            let indices = cursor.get("seen").and_then(Value::as_array)
                .filter(|indices| indices.len() <= 16).ok_or(CURSOR)?;
            for index in indices {
                let index = index.as_u64().filter(|index| *index < 16).ok_or(CURSOR)?;
                let bit = 1_u16 << index;
                if seen & bit != 0 { return Err(CURSOR.into()); }
                seen |= bit;
            }
            manifest = match cursor.get("manifest") {
                Some(Value::Null) if seen == 0 => None,
                Some(Value::String(value)) if value.len() == 64
                    && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) => Some(value.clone()),
                _ => return Err(CURSOR.into()),
            };
        }
        Ok(Self { id: id.to_owned(), seen, manifest, timeout: Duration::from_millis(timeout) })
    }
}

fn record(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    value.as_object().ok_or_else(|| PROTOCOL.into())
}

fn count(value: &Value, name: &str) -> Result<usize> {
    value.get(name).and_then(Value::as_u64).filter(|value| *value <= 16)
        .and_then(|value| usize::try_from(value).ok()).ok_or_else(|| PROTOCOL.into())
}

fn error_flag(value: &Value) -> Result<bool> {
    if !value.get("content").is_some_and(Value::is_array) || value.get("details").is_none() {
        return Err(PROTOCOL.into());
    }
    match value.get("isError") {
        None => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        _ => Err(PROTOCOL.into()),
    }
}

#[derive(Debug)]
struct Observation<'a> {
    status: &'a str,
    items: Option<&'a [Value]>,
    progress: bool,
    mask: u16,
    manifest: Option<String>,
}

#[derive(Serialize)]
struct Item<'a> {
    index: usize,
    key: &'a Value,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Cursor<'a> {
    schema: &'static str,
    job_id: &'a str,
    manifest: Option<&'a str>,
    seen: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Reply<'a> {
    schema: &'static str,
    job_id: &'a str,
    job_status: &'a str,
    done: bool,
    job_failed: bool,
    complete: bool,
    evidence_available: bool,
    total: Option<usize>,
    settled: Option<u32>,
    items: Vec<Item<'a>>,
    cursor: Cursor<'a>,
}

impl<'a> Observation<'a> {
    fn parse(job: &'a Value, request: &Request) -> Result<Self> {
        if job["schema"] != "pi.host_job.v1" || job["id"] != request.id
            || job["command"] != "eval judgment_batch"
        { return Err(PROTOCOL.into()); }
        let status = job.get("status").and_then(Value::as_str).filter(|status| {
            matches!(*status, "running" | "exited" | "failed" | "killed" | "timedOut")
        }).ok_or(PROTOCOL)?;
        let empty = Self { status, items: None, progress: false, mask: 0, manifest: None };
        let Some(result) = job.get("result").filter(|result| !result.is_null()) else {
            return if status == "running" { Ok(empty) } else { Err(PROTOCOL.into()) };
        };
        let failed = error_flag(result)?;
        if (status == "running" || status == "exited") == failed { return Err(PROTOCOL.into()); }
        let details = &result["details"];
        if details.is_null() {
            return if failed { Ok(empty) } else { Err(PROTOCOL.into()) };
        }
        let schema = details.get("schema").and_then(Value::as_str).ok_or(PROTOCOL)?;
        let (payload, receipts, progress) = match schema {
            "pi.host_job.progress.v1" | "pi.host_job.interrupted.v1" => {
                if (schema == "pi.host_job.progress.v1" && status != "running")
                    || (schema == "pi.host_job.interrupted.v1" && !matches!(status, "killed" | "timedOut"))
                { return Err(PROTOCOL.into()); }
                let partial = &details["partialResult"];
                if error_flag(partial)? { return Err(PROTOCOL.into()); }
                let payload = &partial["details"];
                if payload["schema"] != "pi.eval.model-job.progress.v1"
                    || payload["operation"] != "judgment_batch" || payload["complete"] != false
                { return Err(PROTOCOL.into()); }
                (payload, &payload["judgments"], true)
            }
            "pi.eval.model-job.result.v1" if status != "running" => {
                if details["operation"] != "judgment_batch" || details.get("result").is_none() {
                    return Err(PROTOCOL.into());
                }
                let payload = &details["result"];
                if payload.is_null() {
                    return if failed && details["error"].as_str().is_some_and(|s| !s.is_empty()) {
                        Ok(empty)
                    } else { Err(PROTOCOL.into()) };
                }
                if payload["schema"] != "pi.eval.judgment-batch.v1" { return Err(PROTOCOL.into()); }
                (payload, &details["judgments"], false)
            }
            _ => return Err(PROTOCOL.into()),
        };
        let total = count(payload, "total")?;
        let items = payload.get("items").and_then(Value::as_array)
            .filter(|items| total > 0 && items.len() == total).ok_or(PROTOCOL)?;
        if receipts["schema"] != "pi.eval.judgments.v1"
            || !receipts["attempts"].as_array().is_some_and(|items| items.len() == total && items.iter().all(Value::is_object))
        { return Err(PROTOCOL.into()); }
        let mut mask = 0_u16;
        let mut counts = [0_usize; 4]; // queued, running, completed, failed
        let mut keys = Vec::with_capacity(total);
        for (index, item) in items.iter().enumerate() {
            record(item)?;
            let key = item.get("key").ok_or(PROTOCOL)?;
            if !(key.as_str().is_some_and(|key| !key.is_empty() && key.len() <= 128)
                || key.as_u64() == u64::try_from(index).ok()) || keys.contains(&key)
                || keys.first().is_some_and(|first: &&Value| first.is_string() != key.is_string())
            { return Err(PROTOCOL.into()); }
            keys.push(key);
            let state = if progress {
                match item["status"].as_str() {
                    Some("queued") => 0, Some("running") => 1,
                    Some("completed") => 2, Some("failed") => 3,
                    _ => return Err(PROTOCOL.into()),
                }
            } else {
                match item["ok"].as_bool() { Some(true) => 2, Some(false) => 3, _ => return Err(PROTOCOL.into()) }
            };
            counts[state] += 1;
            match state {
                2 => {
                    if item.get("error").is_some() || item["verdict"]["schema"] != "pi.judgment.v1"
                        || !item["verdict"]["answers"].as_object().is_some_and(|answers| !answers.is_empty())
                    { return Err(PROTOCOL.into()); }
                }
                3 => {
                    if item.get("verdict").is_some() || !item["error"].as_str().is_some_and(|text| !text.is_empty()) {
                        return Err(PROTOCOL.into());
                    }
                }
                _ if item.get("verdict").is_some() || item.get("error").is_some() => return Err(PROTOCOL.into()),
                _ => {}
            }
            if state >= 2 { mask |= 1_u16 << index; }
        }
        if count(payload, "succeeded")? != counts[2] || count(payload, "failed")? != counts[3] {
            return Err(PROTOCOL.into());
        }
        if progress {
            if count(payload, "queued")? != counts[0] || count(payload, "running")? != counts[1]
                || count(payload, "completed")? != counts[2] + counts[3]
            { return Err(PROTOCOL.into()); }
        } else if payload["allOk"].as_bool() != Some(counts[3] == 0) || failed != (counts[3] > 0) {
            return Err(PROTOCOL.into());
        }
        // Identical literal keys and order are required across observations,
        // including string "0" versus numeric 0. No model answer enters this digest.
        let manifest = crate::package_manager::hex_encode(&Sha256::digest(
            serde_json::to_vec(&keys).map_err(|_| PROTOCOL)?,
        ));
        if request.seen & !mask != 0 || request.manifest.as_ref().is_some_and(|previous| previous != &manifest) {
            return Err(CURSOR.into());
        }
        Ok(Self { status, items: Some(items), progress, mask, manifest: Some(manifest) })
    }

    fn ready(&self, request: &Request) -> bool {
        self.status != "running" || self.mask & !request.seen != 0
    }

    fn reply(&self, request: &Request) -> Result<String> {
        let mut next = request.seen;
        let mut delta = Vec::new();
        for (index, item) in self.items.into_iter().flatten().enumerate() {
            let bit = 1_u16 << index;
            if self.mask & bit == 0 || request.seen & bit != 0 { continue; }
            let ok = if self.progress { item["status"] == "completed" } else { item["ok"] == true };
            delta.push(Item {
                index, key: &item["key"], ok,
                verdict: ok.then_some(&item["verdict"]),
                error: (!ok).then_some(&item["error"]),
            });
            next |= bit;
        }
        // Borrow accepted bodies until the bounded encoder finishes. An
        // oversized custom host result cannot force a second unbounded copy.
        let value = Reply {
            schema: "pi.eval.batch-drain.v1", job_id: &request.id, job_status: self.status,
            done: self.status != "running", job_failed: !matches!(self.status, "running" | "exited"),
            complete: self.items.is_some() && !self.progress, evidence_available: self.items.is_some(),
            total: self.items.map(|items| items.len()), settled: self.items.map(|_| self.mask.count_ones()),
            items: delta, cursor: Cursor {
                schema: CURSOR_SCHEMA, job_id: &request.id,
                manifest: self.manifest.as_deref().or(request.manifest.as_deref()),
                seen: (0..16).filter(|index| next & (1_u16 << index) != 0).collect(),
            },
        };
        let mut encoded = Bounded(Vec::new());
        serde_json::to_writer(&mut encoded, &value).map_err(|_| "EVAL_BATCH_LIMIT: drain exceeds 1 MiB; cursor unchanged")?;
        String::from_utf8(encoded.0).map_err(|_| PROTOCOL.into())
    }
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (1024 * 1024_usize).saturating_sub(self.0.len()) {
            return Err(io::Error::other("batch drain limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

// Unprojected native snapshots are ONLY wake-up hints. Never return their
// values, identifiers or diagnostics to the guest. A current host lookup is
// mandatory before constructing a response after waiting.
fn ready_hint(snapshot: &crate::jobs::JobSnapshot, seen: u16) -> bool {
    if snapshot.status != "running" { return true; }
    let items = snapshot.result.as_ref().and_then(|result| result.details.as_ref())
        .and_then(|details| details.pointer("/partialResult/details/items")).and_then(Value::as_array);
    items.is_some_and(|items| items.len() > 16 || items.iter().enumerate().any(|(index, item)| {
        seen & (1_u16 << index) == 0 && matches!(item["status"].as_str(), Some("completed" | "failed"))
    }))
}

impl Invocation<'_> {
    pub(super) async fn drain_batch(&mut self, tool: &EvalTool, input: Value) -> Result<String> {
        let request = Request::parse(&input)?;
        let host = self.host.ok_or(HOST)?;
        let session = self.host_session.clone().ok_or(HOST)?;
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| "EVAL_BATCH_CANCELLED")?;
        if !owner.capabilities().io || !owner.capabilities().time { return Err(HOST.into()); }
        let timeout = request.timeout;
        if !timeout.is_zero() && self.tool_receipts.len() > super::MAX_TOOL_CALLS - 2 {
            return Err("EVAL_BRIDGE_LIMIT: a blocking batch drain requires room for two host lookups".into());
        }
        let read = async {
            tool.require_eval_session(&session).await.map_err(|_| HOST)?;
            let initial = self.call_host_output(host, "jobs".into(), json!({
                "action":"wait", "jobId":request.id, "timeoutMs":0,
            })).await.map_err(|_| HOST)?;
            if initial.is_error { return Err(HOST.into()); }
            owner.checkpoint().map_err(|_| "EVAL_BATCH_CANCELLED")?;
            tool.require_eval_session(&session).await.map_err(|_| HOST)?;
            let job = initial.details.as_ref().ok_or(PROTOCOL)?;
            let observed = Observation::parse(job, &request)?;
            if timeout.is_zero() || observed.ready(&request) { return observed.reply(&request); }
            loop {
                owner.checkpoint().map_err(|_| "EVAL_BATCH_CANCELLED")?;
                tool.require_eval_session(&session).await.map_err(|_| HOST)?;
                let native = crate::jobs::wait_async(&session, &request.id, POLL_INTERVAL).await
                    .map_err(|_| "EVAL_BATCH_UNAVAILABLE: native job is no longer available")?;
                if ready_hint(&native, request.seen) { break; }
            }
            tool.require_eval_session(&session).await.map_err(|_| HOST)?;
            let current = self.call_host_output(host, "jobs".into(), json!({
                "action":"wait", "jobId":request.id, "timeoutMs":0,
            })).await.map_err(|_| HOST)?;
            if current.is_error { return Err(HOST.into()); }
            owner.checkpoint().map_err(|_| "EVAL_BATCH_CANCELLED")?;
            tool.require_eval_session(&session).await.map_err(|_| HOST)?;
            Observation::parse(current.details.as_ref().ok_or(PROTOCOL)?, &request)?.reply(&request)
        };
        if timeout.is_zero() { return owner.with_current(read).await; }
        owner.with_current(crate::text_completion::with_timeout(timeout, read)).await.map_err(|stop| match stop {
            crate::text_completion::RequestStop::TimedOut => "EVAL_BATCH_TIMEOUT: no drain delivered; retain the cursor; the model job is not cancelled".to_owned(),
            crate::text_completion::RequestStop::Cancelled => "EVAL_BATCH_CANCELLED: cursor unchanged".to_owned(),
            crate::text_completion::RequestStop::TimeUnavailable => HOST.to_owned(),
        })?
    }
}

#[cfg(test)]
mod tests;
