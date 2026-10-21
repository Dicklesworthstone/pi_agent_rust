//! Explicit completion jobs share the native session-owned jobs lifecycle.
//!
//! Only an invocation with an owned-privacy factory may transfer work out of a
//! cell. Neither the factory, the live projection, the Agent host nor a kernel
//! enters the job. Inspecting/cancelling later uses the ordinary jobs tool.

use super::{AgentCx, Invocation, MAX_CALLS, completion};
use crate::error::{Error, Result};
use crate::eval::EvalTool;
use crate::judgment::{CompletionRequest, Judge};
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub(super) const BRIDGE_NAME: &str = "__pi_complete_start";
const JOB_BUDGET: Duration = Duration::from_secs(60);

fn failure(code: &str) -> Error {
    Error::tool("eval", code)
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| failure("PI_COMPLETION_CANCELLED"))?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(failure("PI_COMPLETION_PERMISSION"));
    }
    Ok(())
}

impl Invocation<'_> {
    pub(super) async fn start_completion(
        &mut self,
        tool: &EvalTool,
        input: Value,
    ) -> std::result::Result<String, String> {
        let request = CompletionRequest::parse(input).map_err(completion::public_error)?;
        if self.host.is_none() || self.snapshot_privacy.is_none() || self.host_session.is_none() {
            return Err("PI_COMPLETION_BACKGROUND_CONTEXT: background completions require an owning Agent invocation with an owned privacy snapshot".into());
        }
        if self.judge.is_none() && self.selection.is_none() {
            return Err("PI_COMPLETION_UNBOUND: supply this cell's judge_model or bind a Judge in the host".into());
        }
        if self.receipts.len() >= MAX_CALLS {
            return Err("PI_COMPLETION_LIMIT: completions and judgments share 16 attempts per eval cell".into());
        }
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner).map_err(public_error)?;
        let started = Instant::now();
        let index = self.receipts.len();
        self.receipts.push(json!({
            "attempt": index + 1, "operation": "completion_background",
            "selectedModel": self.selection, "status": "launching",
            "usage": null, "usageKnown": false,
        }));
        let outcome = self.admit_completion(tool, request, owner, started).await;
        // No await after publication: retain the ID even if the interpreter
        // cannot receive its acknowledgment. The parent can inspect jobs/receipts.
        let receipt = self.receipts.get_mut(index).and_then(Value::as_object_mut).ok_or_else(|| {
            "PI_COMPLETION_RECEIPT: native launch receipt unavailable".to_string()
        })?;
        match outcome {
            Ok(id) => {
                receipt.insert("status".into(), json!("background"));
                receipt.insert("jobId".into(), json!(id));
                Ok(json!({
                    "schema": "pi.completion.background.v1",
                    "jobId": id,
                    "resultSchema": "pi.completion.v1",
                    "budgetSeconds": JOB_BUDGET.as_secs(),
                }).to_string())
            }
            Err(error) => {
                let error = public_error(error);
                receipt.insert("status".into(), json!("failed"));
                receipt.insert("error".into(), json!(error));
                Err(error)
            }
        }
    }

    async fn admit_completion(
        &mut self,
        tool: &EvalTool,
        request: CompletionRequest,
        owner: AgentCx,
        started: Instant,
    ) -> Result<String> {
        let session = self.host_session.clone()
            .ok_or_else(|| failure("PI_COMPLETION_BACKGROUND_CONTEXT"))?;
        // Capture the shutdown generation before any asynchronous preparation.
        // Neither changing the live session nor closing/reopening it can make
        // old admission publish under a new owner generation.
        let admission = crate::jobs::admit_owned_task(&session)?;
        tool.require_eval_session(&session).await
            .map_err(|_| failure("PI_COMPLETION_SESSION_CHANGED"))?;
        let judge = self.bound_judge(&tool.cwd).await?;
        check_owner(&owner)?;
        let snapshot = (self.snapshot_privacy
            .ok_or_else(|| failure("PI_COMPLETION_BACKGROUND_CONTEXT"))?)();
        tool.require_eval_session(&session).await
            .map_err(|_| failure("PI_COMPLETION_SESSION_CHANGED"))?;
        check_owner(&owner)?;
        let remaining = JOB_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(failure("PI_COMPLETION_TIMEOUT"));
        }
        // The snapshot is owned data. No reference to self/tool/host/protect is
        // captured by the 'static future accepted by the native jobs runtime.
        let result_privacy = snapshot.clone();
        let deadline = started + JOB_BUDGET;
        let job = crate::jobs::spawn_owned_task(
            admission, "eval completion", owner, remaining, Some(&tool.completion_artifacts), result_privacy,
            async move { complete_owned(judge, request, snapshot, deadline).await },
        )?;
        Ok(job.id)
    }
}

async fn complete_owned(
    judge: Judge,
    request: CompletionRequest,
    privacy: AuxiliaryPrivacySnapshot,
    deadline: Instant,
) -> Result<ToolOutput> {
    let project = |parts: &[&str]| -> Result<Vec<String>> {
        let projected = privacy.project_json(&json!(parts))?;
        serde_json::from_value(projected)
            .map_err(|_| failure("PI_COMPLETION_PRIVACY"))
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() { return Err(failure("PI_COMPLETION_TIMEOUT")); }
    let reply = crate::text_completion::with_timeout(
        remaining, judge.complete_protected(request, Some(&project)),
    ).await.map_err(|stop| match stop {
        crate::text_completion::RequestStop::TimedOut => failure("PI_COMPLETION_TIMEOUT"),
        crate::text_completion::RequestStop::Cancelled => failure("PI_COMPLETION_CANCELLED"),
        crate::text_completion::RequestStop::TimeUnavailable => failure("PI_COMPLETION_PERMISSION"),
    })?.map_err(|error| failure(&completion::public_error(error)))?;
    if Instant::now() >= deadline { return Err(failure("PI_COMPLETION_TIMEOUT")); }
    // Jobs will screen the output before artifacts/notifications too. Refuse
    // any altered completion here: a redacted typed value must not retain a
    // schemaValid=true claim about a different, unredacted answer.
    let protected = privacy.project_json(&reply)
        .map_err(|_| failure("PI_COMPLETION_PRIVACY: completed result withheld"))?;
    if protected != reply {
        return Err(failure("PI_COMPLETION_PRIVACY: completed result withheld"));
    }
    check_owner(&AgentCx::for_current_or_request())?;
    if Instant::now() >= deadline { return Err(failure("PI_COMPLETION_TIMEOUT")); }
    let text = reply.get("text").and_then(Value::as_str)
        .ok_or_else(|| failure("PI_COMPLETION_RESPONSE"))?.to_owned();
    Ok(ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(reply),
        is_error: false,
    })
}

fn public_error(error: Error) -> String {
    let text = error.to_string();
    for code in [
        "PI_COMPLETION_BACKGROUND_CONTEXT", "PI_COMPLETION_SESSION_CHANGED",
        "PI_COMPLETION_CANCELLED", "PI_COMPLETION_PERMISSION", "PI_COMPLETION_TIMEOUT",
    ] {
        if text.contains(code) {
            return format!("{code}: completion job was not admitted");
        }
    }
    if text.contains("PI_JOBS_") || text.contains("jobs") {
        // Artifact paths, foreign IDs and host diagnostics are not guest data.
        return "PI_COMPLETION_BACKGROUND: native jobs admission failed; no handle".into();
    }
    completion::public_error(error)
}

#[cfg(test)]
mod tests;
