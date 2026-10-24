//! Retained, tool-free auxiliary model jobs admitted by the current eval cell.
//!
//! All source reads and privacy preparation finish before a native job may
//! start. The retained future owns prepared inputs and a bound provider; it
//! never owns an Agent borrow, tool host, interpreter or privacy callback.

use super::{Invocation, MAX_CALLS, MAX_TOOL_CALLS, batch, completion, images, public_error};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::judgment::{
    CompletionRequest, Judge, JudgmentRequest, PreparedCompletion, PreparedJudgment,
};
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::ToolOutput;
use futures::channel::oneshot;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const ACK_SCHEMA: &str = "pi.eval.model-job.v1";
const RESULT_SCHEMA: &str = "pi.eval.model-job.result.v1";
const PROGRESS_SCHEMA: &str = "pi.eval.model-job.progress.v1";
const MAX_TEXT_BYTES: usize = 256 * 1024;
// Leave room for receipts and the native ToolOutput envelope under jobs' 1 MiB.
const MAX_RESULT_BYTES: usize = 768 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    LegacyCompletion,
    Completion,
    Judgment,
    Batch,
}

impl Operation {
    pub(super) fn for_bridge(name: &str) -> Option<Self> {
        match name {
            "__pi_complete_start" => Some(Self::LegacyCompletion),
            "__pi_model_complete_start" => Some(Self::Completion),
            "__pi_judge_start" => Some(Self::Judgment),
            "__pi_judge_batch_start" => Some(Self::Batch),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::LegacyCompletion | Self::Completion => "completion",
            Self::Judgment => "judgment",
            Self::Batch => "judgment_batch",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::LegacyCompletion => "eval completion",
            Self::Completion => "eval model completion",
            Self::Judgment => "eval judgment",
            Self::Batch => "eval judgment_batch",
        }
    }

    fn error(self, error: Error) -> String {
        if matches!(self, Self::LegacyCompletion | Self::Completion) {
            completion::public_error(error)
        } else {
            public_error(error)
        }
    }

    fn admission_error(self, error: String) -> String {
        if self != Self::LegacyCompletion {
            return error;
        }
        for (source, target) in [
            ("PI_MODEL_JOB_HOST", "PI_COMPLETION_BACKGROUND_CONTEXT"),
            ("PI_MODEL_JOB_UNBOUND", "PI_COMPLETION_UNBOUND"),
            ("PI_MODEL_JOB_LIMIT", "PI_COMPLETION_LIMIT"),
            ("PI_MODEL_JOB_CALL_LIMIT", "PI_COMPLETION_LIMIT"),
            ("PI_MODEL_JOB_CANCELLED", "PI_COMPLETION_CANCELLED"),
            ("PI_MODEL_JOB_TIMEOUT", "PI_COMPLETION_TIMEOUT"),
            ("PI_MODEL_JOB_PERMISSION", "PI_COMPLETION_PERMISSION"),
            (
                "PI_MODEL_JOB_SESSION_CHANGED",
                "PI_COMPLETION_SESSION_CHANGED",
            ),
            ("PI_MODEL_JOB_PRIVACY", "PI_COMPLETION_PRIVACY"),
            ("PI_MODEL_JOB_STOPPED", "PI_COMPLETION_BACKGROUND"),
            ("PI_MODEL_JOB_START", "PI_COMPLETION_BACKGROUND"),
        ] {
            if error.contains(source) {
                return format!("{target}: completion job was not admitted");
            }
        }
        error
    }
}

enum Request {
    Completion(CompletionRequest, images::Sources),
    Judgment(JudgmentRequest, images::Sources),
    Batch(batch::Plan, BatchOptions),
}

#[derive(Clone, Copy)]
struct BatchOptions {
    retries: usize,
    min_ok: usize,
}

impl Request {
    fn parse(
        operation: Operation,
        input: Value,
    ) -> std::result::Result<(Self, Value, Duration), String> {
        let Value::Object(mut fields) = input else {
            return Err(input_error());
        };
        if operation == Operation::LegacyCompletion
            && (fields.contains_key("timeout_secs") || fields.contains_key("images"))
        {
            return Err("PI_COMPLETION_INPUT: raw completion jobs accept ordinary text completion fields only".into());
        }
        let timeout = match fields.remove("timeout_secs") {
            None => {
                if operation == Operation::Batch {
                    300
                } else {
                    60
                }
            }
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=600).contains(seconds))
                .ok_or_else(input_error)?,
        };
        let image_input = fields.remove("images");
        let (retries, min_ok) = if operation == Operation::Batch {
            (fields.remove("retries"), fields.remove("min_ok"))
        } else {
            (None, None)
        };
        let text = Value::Object(fields);
        // Check the complete text before making a request copy. Image parsers
        // separately bound their borrowed descriptors before copying payloads.
        batch::encoded_size(&text, MAX_TEXT_BYTES)
            .map_err(|_| "PI_MODEL_JOB_INPUT_LIMIT: request text exceeds 256 KiB".to_string())?;
        let request = match operation {
            Operation::LegacyCompletion | Operation::Completion => {
                if operation == Operation::LegacyCompletion && image_input.is_some() {
                    return Err(input_error());
                }
                Self::Completion(
                    CompletionRequest::parse(text.clone()).map_err(completion::public_error)?,
                    images::Sources::parse(image_input.as_ref())
                        .map_err(completion::public_error)?,
                )
            }
            Operation::Judgment => {
                let request = JudgmentRequest::parse(text.clone()).map_err(public_error)?;
                let sources = images::Sources::parse(image_input.as_ref()).map_err(public_error)?;
                Self::Judgment(request, sources)
            }
            Operation::Batch => {
                let mut batch_input = text.clone();
                if let Some(images) = image_input {
                    batch_input
                        .as_object_mut()
                        .ok_or_else(input_error)?
                        .insert("images".into(), images);
                }
                let plan = batch::Plan::parse(&batch_input)?;
                let retries = match retries {
                    None => 0,
                    Some(value) => value
                        .as_u64()
                        .filter(|value| *value <= 3)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or_else(input_error)?,
                };
                let min_ok = match min_ok {
                    None => plan.items.len(),
                    Some(value) => value
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .filter(|value| *value <= plan.items.len())
                        .ok_or_else(input_error)?,
                };
                Self::Batch(plan, BatchOptions { retries, min_ok })
            }
        };
        Ok((request, text, Duration::from_secs(timeout)))
    }

    fn count(&self) -> usize {
        match self {
            Self::Batch(plan, options) => plan.items.len() * (options.retries + 1),
            _ => 1,
        }
    }

    fn path_count(&self) -> usize {
        match self {
            Self::Completion(_, images) | Self::Judgment(_, images) => images.path_count(),
            Self::Batch(plan, _) => plan.items.iter().map(|item| item.images.path_count()).sum(),
        }
    }

    fn has_images(&self) -> bool {
        match self {
            Self::Completion(_, images) | Self::Judgment(_, images) => !images.is_empty(),
            Self::Batch(plan, _) => plan.items.iter().any(|item| !item.images.is_empty()),
        }
    }
}

fn input_error() -> String {
    "PI_MODEL_JOB_INPUT: use ordinary completion/judgment fields and optional timeout_secs from 1 to 600; batches accept retries from 0 to 3 and min_ok from 0 through item count".into()
}

fn stopped() -> Error {
    Error::tool(
        "eval",
        "PI_MODEL_JOB_STOPPED: model job admission stopped before provider dispatch",
    )
}

fn checkpoint(owner: &AgentCx, deadline: Instant) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| Error::tool("eval", "PI_MODEL_JOB_CANCELLED"))?;
    if Instant::now() >= deadline {
        return Err(Error::tool("eval", "PI_MODEL_JOB_TIMEOUT"));
    }
    let capabilities = owner.capabilities();
    if !capabilities.spawn || !capabilities.io || !capabilities.time {
        return Err(Error::tool(
            "eval",
            "PI_MODEL_JOB_PERMISSION: model jobs require spawn, I/O and timer capabilities",
        ));
    }
    Ok(())
}

enum Attached {
    Completion(CompletionRequest),
    Judgment(JudgmentRequest),
    Batch(Vec<(Value, JudgmentRequest)>, usize, BatchOptions),
}

enum Prepared {
    Completion(PreparedCompletion),
    Judgment(PreparedJudgment),
    Batch(Vec<(Value, PreparedJudgment)>, usize, BatchOptions),
}

impl Attached {
    fn prepare(self, judge: &Judge, privacy: &AuxiliaryPrivacySnapshot) -> Result<Prepared> {
        let protect = |parts: &[&str]| privacy.project_inputs(parts);
        match self {
            Self::Completion(request) => judge
                .prepare_completion(request, Some(&protect))
                .map(Prepared::Completion),
            Self::Judgment(request) => judge
                .prepare_judgment(request, Some(&protect))
                .map(Prepared::Judgment),
            Self::Batch(items, concurrency, options) => {
                let mut prepared = Vec::with_capacity(items.len());
                for (key, request) in items {
                    if privacy.project_json(&key)? != key {
                        return Err(Error::tool(
                            "judge",
                            "PI_JUDGE_PRIVACY: privacy projection changed a batch key",
                        ));
                    }
                    prepared.push((key, judge.prepare_judgment(request, Some(&protect))?));
                }
                Ok(Prepared::Batch(prepared, concurrency, options))
            }
        }
    }
}

impl Invocation<'_> {
    /// Reserve all attempts in the launching cell. Later cells can inspect or
    /// cancel the job, but cannot renew its deadline, binding or attempt budget.
    pub(super) async fn start_background(
        &mut self,
        tool: &crate::eval::EvalTool,
        operation: Operation,
        input: Value,
    ) -> std::result::Result<String, String> {
        self.admit_background(tool, operation, input)
            .await
            .map_err(|error| operation.admission_error(error))
    }

    async fn admit_background(
        &mut self,
        tool: &crate::eval::EvalTool,
        operation: Operation,
        mut input: Value,
    ) -> std::result::Result<String, String> {
        let selection = self
            .request_selection(&mut input)
            .map_err(|error| operation.error(error))?;
        let (request, text, timeout) = Request::parse(operation, input)?;
        let snapshot = self.snapshot_privacy.ok_or_else(|| {
            "PI_MODEL_JOB_HOST: background models require the current Agent invocation".to_string()
        })?;
        let host = self.host.ok_or_else(|| {
            "PI_MODEL_JOB_HOST: background models require the current Agent tool host".to_string()
        })?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err(
                "PI_MODEL_JOB_UNBOUND: select this cell's judge_model or bind a Judge in the host"
                    .into(),
            );
        }
        let count = request.count();
        let item_count = match &request {
            Request::Batch(plan, _) => plan.items.len(),
            _ => 1,
        };
        if count > MAX_CALLS.saturating_sub(self.receipts.len()) {
            return Err("PI_MODEL_JOB_LIMIT: foreground and background models share 16 attempts per eval cell".into());
        }
        // One policy-checked jobs admission plus all image reads must fit before
        // entering any host. No prefix read occurs on an over-budget request.
        if request.path_count().saturating_add(1)
            > MAX_TOOL_CALLS.saturating_sub(self.tool_receipts.len())
        {
            return Err("PI_MODEL_JOB_CALL_LIMIT: jobs admission and image reads exceed this cell's host-call budget".into());
        }
        let owner_id = self
            .host_session
            .clone()
            .ok_or_else(|| "PI_MODEL_JOB_HOST: no captured session owner".to_string())?;
        let owner = AgentCx::for_current_or_request();
        let deadline = Instant::now() + timeout;
        checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
        let admission = crate::jobs::admit_owned_task(&owner_id)
            .map_err(|_| "PI_MODEL_JOB_STOPPED: session cannot admit background work".to_string())?
            .with_deadline(deadline);
        let base = self.receipts.len();
        for index in 0..count {
            let mut receipt = json!({
                "attempt": base + index + 1,
                "operation": if operation == Operation::LegacyCompletion { "completion_background" } else { operation.name() },
                "background": true, "reserved": true, "selectedModel": selection,
                "status": "not_started", "usage": null, "usageKnown": false,
            });
            // Retry slots have no item until a concrete failed attempt queues
            // them. Their identity must agree with the eventual job receipts.
            if index < item_count {
                receipt["item"] = json!(index);
                receipt["itemAttempt"] = json!(1);
            }
            self.receipts.push(receipt);
        }
        let outcome = async {
            // Reuse current registry, approval, plan and session policy. This
            // prevents starting work whose ordinary jobs controls are disabled.
            let jobs = self
                .call_host_output(host, "jobs".into(), json!({"action":"list"}))
                .await
                .map_err(|_| "PI_MODEL_JOB_HOST: jobs admission was refused".to_string())?;
            if jobs.is_error {
                return Err("PI_MODEL_JOB_HOST: jobs admission was refused".to_string());
            }
            checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
            tool.require_eval_session(&owner_id).await.map_err(|_| {
                "PI_MODEL_JOB_SESSION_CHANGED: owner changed during admission".to_string()
            })?;
            let judge = self
                .bound_judge(&tool.cwd, selection.as_ref())
                .await
                .map_err(|error| operation.error(error))?;
            judge
                .check_image_support(request.has_images())
                .map_err(|error| operation.error(error))?;
            self.check_image_reads(request.path_count())
                .map_err(|error| operation.error(error))?;
            let attached = self.attach_background(request, &owner, deadline).await?;
            checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
            // Capture after all reads, then discover across the complete text
            // batch before preparing even its first item. Later assignments
            // protect earlier bare echoes and the retained provider result.
            let mut privacy = snapshot();
            privacy
                .capture_json(&text)
                .map_err(|error| operation.error(error))?;
            let prepared = attached
                .prepare(&judge, &privacy)
                .map_err(|error| operation.error(error))?;
            let label = Value::String(operation.label().into());
            if privacy
                .project_json(&label)
                .map_err(|error| operation.error(error))?
                != label
            {
                return Err(
                    "PI_MODEL_JOB_PRIVACY: privacy projection changed the job identity".into(),
                );
            }
            checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
            tool.require_eval_session(&owner_id).await.map_err(|_| {
                "PI_MODEL_JOB_SESSION_CHANGED: owner changed before launch".to_string()
            })?;
            checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
            let (start_tx, start_rx) = oneshot::channel::<JobProgress>();
            let result_privacy = privacy.clone();
            let job = crate::jobs::spawn_owned_task(
                admission,
                operation.label(),
                owner.clone(),
                deadline.saturating_duration_since(Instant::now()),
                Some(&tool.completion_artifacts),
                privacy.clone(),
                async move {
                    // Dropping the launching cell before a screened ACK closes
                    // the gate. No provider request can escape that admission.
                    let progress = start_rx.await.map_err(|_| stopped())?;
                    prepared
                        .run(
                            judge,
                            operation,
                            base,
                            result_privacy,
                            deadline,
                            Some(progress),
                        )
                        .await
                },
            )
            .map_err(|_| {
                "PI_MODEL_JOB_START: native job could not be admitted; no provider started"
                    .to_string()
            })?;
            let acknowledgment = if operation == Operation::LegacyCompletion {
                json!({
                    "schema":"pi.completion.background.v1", "jobId":job.id,
                    "resultSchema":"pi.completion.v1", "budgetSeconds":timeout.as_secs(),
                })
            } else {
                json!({
                    "schema": ACK_SCHEMA, "operation": operation.name(),
                    "jobId": job.id, "timeoutSeconds": timeout.as_secs(),
                })
            };
            // Preserve the ID in native receipts even if the guest loses its
            // acknowledgment. No caller retries an indeterminate admission.
            for receipt in self.receipts.iter_mut().skip(base).take(count) {
                if let Some(receipt) = receipt.as_object_mut() {
                    receipt.insert("jobId".into(), json!(job.id));
                }
            }
            if privacy
                .project_json(&acknowledgment)
                .map_err(|error| operation.error(error))?
                != acknowledgment
            {
                return Err(
                    "PI_MODEL_JOB_PRIVACY: privacy projection changed the job acknowledgment"
                        .into(),
                );
            }
            let encoded = serde_json::to_string(&acknowledgment)
                .map_err(|_| "PI_MODEL_JOB_RESPONSE: cannot encode acknowledgment".to_string())?;
            tool.require_eval_session(&owner_id).await.map_err(|_| {
                "PI_MODEL_JOB_SESSION_CHANGED: owner changed before acknowledgment".to_string()
            })?;
            checkpoint(&owner, deadline).map_err(|error| error.to_string())?;
            crate::jobs::with_owned_task_progress(&owner_id, &job.id, || {
                start_tx
                    .send(JobProgress {
                        owner: owner_id.clone(),
                        id: job.id.clone(),
                    })
                    .map_err(|_| stopped())?;
                Ok(((), None))
            })
            .map_err(|_| {
                "PI_MODEL_JOB_STOPPED: native job stopped before provider dispatch".to_string()
            })?;
            Ok(encoded)
        }
        .await
        .map_err(|error| operation.admission_error(error));
        for receipt in self.receipts.iter_mut().skip(base).take(count) {
            if let Some(receipt) = receipt.as_object_mut() {
                receipt.insert(
                    "status".into(),
                    json!(if outcome.is_ok() {
                        "background"
                    } else if operation == Operation::LegacyCompletion {
                        "failed"
                    } else {
                        "not_started"
                    }),
                );
                if let Err(error) = &outcome {
                    let field = if operation == Operation::LegacyCompletion {
                        "error"
                    } else {
                        "admissionError"
                    };
                    receipt.insert(field.into(), json!(error));
                }
            }
        }
        outcome
    }

    async fn attach_background(
        &mut self,
        request: Request,
        owner: &AgentCx,
        deadline: Instant,
    ) -> std::result::Result<Attached, String> {
        match request {
            Request::Completion(request, sources) => {
                let images = self
                    .load_images(sources)
                    .await
                    .map_err(completion::public_error)?;
                request
                    .with_images(images)
                    .map(Attached::Completion)
                    .map_err(completion::public_error)
            }
            Request::Judgment(request, images) => self
                .attach_images(request, images)
                .await
                .map(Attached::Judgment)
                .map_err(public_error),
            Request::Batch(plan, options) => {
                let mut attached = Vec::with_capacity(plan.items.len());
                let mut bytes = 0_usize;
                for item in plan.items {
                    checkpoint(owner, deadline).map_err(|error| error.to_string())?;
                    let request = self
                        .attach_images(item.request, item.images)
                        .await
                        .map_err(public_error)?;
                    bytes = bytes
                        .checked_add(request.image_bytes())
                        .filter(|bytes| *bytes <= images::MAX_BATCH_IMAGE_BYTES)
                        .ok_or_else(|| public_error(images::limit()))?;
                    attached.push((item.key, request));
                }
                Ok(Attached::Batch(attached, plan.concurrency, options))
            }
        }
    }
}

fn attempt(index: usize, operation: Operation, outcome: &Result<Value>) -> Value {
    let mut receipt = json!({
        "attempt": index + 1, "operation": operation.name(),
        "status": "failed", "usage": null, "usageKnown": false,
    });
    if let Some(fields) = receipt.as_object_mut() {
        match outcome {
            Ok(value) => {
                fields.insert("status".into(), json!("completed"));
                for name in ["provider", "modelId", "api", "usage"] {
                    fields.insert(name.into(), value.get(name).cloned().unwrap_or(Value::Null));
                }
                fields.insert(
                    "usageKnown".into(),
                    json!(value.get("usage").is_some_and(|usage| !usage.is_null())),
                );
            }
            Err(error) => {
                fields.insert(
                    "error".into(),
                    json!(operation.error(Error::tool("eval", error.to_string()))),
                );
            }
        }
    }
    receipt
}

fn output(
    operation: Operation,
    result: Value,
    receipts: Vec<Value>,
    error: Option<String>,
    failed: bool,
    privacy: &AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    if operation == Operation::LegacyCompletion && failed {
        return Err(Error::tool(
            "eval",
            error.unwrap_or_else(|| "PI_COMPLETION_FAILED".into()),
        ));
    }
    let text = if operation == Operation::LegacyCompletion {
        result
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("eval", "PI_COMPLETION_RESPONSE"))?
            .to_owned()
    } else {
        format!(
            "Background {} {}. The structured native result and attempt receipts are in details.",
            operation.name(),
            if failed { "failed" } else { "completed" },
        )
    };
    let details = if operation == Operation::LegacyCompletion {
        result
    } else {
        json!({
            "schema": RESULT_SCHEMA, "operation": operation.name(), "result": result,
            "judgments": {"schema":"pi.eval.judgments.v1", "attempts":receipts},
            "error": error,
        })
    };
    batch::encoded_size(&details, MAX_RESULT_BYTES).map_err(|_| {
        Error::tool(
            "eval",
            "PI_MODEL_JOB_OUTPUT_LIMIT: completed model result exceeds 768 KiB",
        )
    })?;
    let output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: failed,
    };
    screen_output(operation, output, privacy)
}

fn screen_output(
    operation: Operation,
    output: ToolOutput,
    privacy: &AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    // Project the complete aggregate, not independent items: a late provider
    // assignment must protect an earlier bare echo. Privacy cannot silently
    // mutate typed data, answer probabilities, keys, receipts or identity and
    // preserve a success/schemaValid claim. Only plain completion text may be
    // redacted in place; every other mutation withholds the structured result.
    let original = serde_json::to_value(&output)?;
    let privacy_error = || {
        Error::tool(
            "eval",
            if operation == Operation::LegacyCompletion {
                "PI_COMPLETION_PRIVACY: completed result withheld"
            } else {
                "PI_MODEL_JOB_RESULT_PRIVACY: result projection changed typed evidence or native identity; no structured result retained"
            },
        )
    };
    let projected = privacy
        .project_json(&original)
        .map_err(|_| privacy_error())?;
    let mut permitted = original;
    if operation == Operation::Completion
        && permitted.pointer("/details/result/data").is_none()
        && let Some(text) = projected
            .pointer("/details/result/text")
            .filter(|text| text.is_string())
        && let Some(target) = permitted.pointer_mut("/details/result/text")
    {
        *target = text.clone();
    }
    if permitted != projected {
        return Err(privacy_error());
    }
    serde_json::from_value(projected).map_err(|_| privacy_error())
}

/// An owned identity, not a host capability. All mutations still enter the
/// jobs registry's current owner/cancellation fence.
struct JobProgress {
    owner: String,
    id: String,
}

impl JobProgress {
    /// A later aggregate can invalidate an earlier screened observation. Clear
    /// synchronously before returning the failure, even if cancellation has
    /// raced it; terminal conversion must not preserve stale typed evidence.
    fn screened<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            let _ = crate::jobs::clear_owned_task_progress(&self.owner, &self.id);
        }
        result
    }

    fn publish(&self, batch: &BatchProgress, privacy: &AuxiliaryPrivacySnapshot) -> Result<()> {
        let partial = self.screened(batch.output(privacy))?;
        // A cancellation refusal keeps the last accepted observation. The
        // publisher separately withdraws progress if its own screening fails.
        crate::jobs::publish_owned_task_progress(&self.owner, &self.id, partial, privacy)
    }
}

enum BatchItem {
    Queued(Value),
    Running(Value),
    Completed { key: Value, verdict: Value },
    Failed { key: Value, error: String },
}

struct BatchProgress {
    items: Vec<BatchItem>,
    receipts: Vec<Value>,
    base: usize,
    options: BatchOptions,
    attempts: Vec<usize>,
    active_receipts: Vec<usize>,
}

impl BatchProgress {
    fn new(items: &[(Value, PreparedJudgment)], base: usize, options: BatchOptions) -> Self {
        Self {
            items: items
                .iter()
                .map(|(key, _)| BatchItem::Queued(key.clone()))
                .collect(),
            receipts: (0..items.len())
                .map(|index| {
                    json!({
                        "attempt":base + index + 1, "operation":"judgment", "status":"queued",
                        "item":index, "itemAttempt":1, "usage":null, "usageKnown":false,
                    })
                })
                .collect(),
            base,
            options,
            attempts: vec![0; items.len()],
            active_receipts: (0..items.len()).collect(),
        }
    }

    fn running(&mut self, index: usize) -> Result<()> {
        let receipt = self
            .receipts
            .get_mut(*self.active_receipts.get(index).ok_or_else(stopped)?)
            .and_then(Value::as_object_mut)
            .ok_or_else(stopped)?;
        let item = self.items.get_mut(index).ok_or_else(stopped)?;
        let BatchItem::Queued(key) = item else {
            return Err(stopped());
        };
        *item = BatchItem::Running(key.clone());
        self.attempts[index] += 1;
        receipt.insert("status".into(), json!("running"));
        Ok(())
    }

    fn completed(
        &mut self,
        index: usize,
        key: Value,
        outcome: Result<Value>,
        retry: bool,
    ) -> Result<()> {
        let item = self.items.get_mut(index).ok_or_else(stopped)?;
        if !matches!(item, BatchItem::Running(expected) if *expected == key) {
            return Err(stopped());
        }
        let receipt_index = *self.active_receipts.get(index).ok_or_else(stopped)?;
        let mut receipt = attempt(self.base + receipt_index, Operation::Judgment, &outcome);
        receipt["item"] = json!(index);
        receipt["itemAttempt"] = json!(self.attempts[index]);
        receipt["retryScheduled"] = json!(retry);
        *self.receipts.get_mut(receipt_index).ok_or_else(stopped)? = receipt;
        if retry {
            if outcome.is_ok() || self.attempts[index] > self.options.retries {
                return Err(stopped());
            }
            let receipt_index = self.receipts.len();
            self.receipts.push(json!({
                "attempt":self.base + receipt_index + 1, "operation":"judgment",
                "item":index, "itemAttempt":self.attempts[index] + 1, "status":"queued",
                "usage":null, "usageKnown":false,
            }));
            self.active_receipts[index] = receipt_index;
            *item = BatchItem::Queued(key);
            return Ok(());
        }
        *item = match outcome {
            Ok(verdict) => BatchItem::Completed { key, verdict },
            Err(error) => BatchItem::Failed {
                key,
                error: public_error(error),
            },
        };
        Ok(())
    }

    fn output(&self, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
        let mut succeeded = 0_usize;
        let mut failed = 0_usize;
        let mut queued = 0_usize;
        let mut running = 0_usize;
        let items: Vec<Value> = self
            .items
            .iter()
            .map(|item| match item {
                BatchItem::Queued(key) => {
                    queued += 1;
                    json!({"key":key,"status":"queued"})
                }
                BatchItem::Running(key) => {
                    running += 1;
                    json!({"key":key,"status":"running"})
                }
                BatchItem::Completed { key, verdict } => {
                    succeeded += 1;
                    json!({"key":key,"status":"completed","verdict":verdict})
                }
                BatchItem::Failed { key, error } => {
                    failed += 1;
                    json!({"key":key,"status":"failed","error":error})
                }
            })
            .collect();
        let details = json!({
            "schema":PROGRESS_SCHEMA, "operation":"judgment_batch", "complete":false,
            "total":items.len(), "completed":succeeded + failed,
            "succeeded":succeeded, "failed":failed, "queued":queued, "running":running,
            "retries":self.options.retries, "minOk":self.options.min_ok,
            "meetsMinimum":succeeded >= self.options.min_ok,
            "items":items,
            "judgments":{"schema":"pi.eval.judgments.v1","attempts":self.receipts},
        });
        batch::encoded_size(&details, MAX_RESULT_BYTES).map_err(|_| {
            Error::tool(
                "eval",
                "PI_MODEL_JOB_OUTPUT_LIMIT: batch progress exceeds 768 KiB",
            )
        })?;
        screen_output(
            Operation::Batch,
            ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Background judgment batch progress: {} of {} items finished.",
                    succeeded + failed,
                    self.items.len(),
                )))],
                details: Some(details),
                is_error: false,
            },
            privacy,
        )
    }

    fn finish(self, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
        let count = self.items.len();
        let mut succeeded = 0_usize;
        let mut results = Vec::with_capacity(count);
        for item in self.items {
            results.push(match item {
                BatchItem::Completed { key, verdict } => {
                    succeeded += 1;
                    json!({"key":key,"ok":true,"verdict":verdict})
                }
                BatchItem::Failed { key, error } => json!({"key":key,"ok":false,"error":error}),
                BatchItem::Queued(_) | BatchItem::Running(_) => return Err(stopped()),
            });
        }
        output(
            Operation::Batch,
            json!({
                "schema":"pi.eval.judgment-batch.v1", "batch":self.base + 1,
                "total":count, "succeeded":succeeded, "failed":count - succeeded,
                "allOk":succeeded == count, "items":results,
                "retries":self.options.retries, "minOk":self.options.min_ok,
                "meetsMinimum":succeeded >= self.options.min_ok,
            }),
            self.receipts,
            None,
            succeeded < self.options.min_ok,
            privacy,
        )
    }
}

impl Prepared {
    async fn run(
        self,
        judge: Judge,
        operation: Operation,
        base: usize,
        privacy: AuxiliaryPrivacySnapshot,
        deadline: Instant,
        progress: Option<JobProgress>,
    ) -> Result<ToolOutput> {
        let owner = AgentCx::for_current_or_request();
        let check = || {
            checkpoint(&owner, deadline)
                .map_err(|error| Error::tool("eval", operation.admission_error(error.to_string())))
        };
        check()?;
        let result = crate::text_completion::with_timeout(
            deadline.saturating_duration_since(Instant::now()),
            self.run_prepared(judge, operation, base, privacy, deadline, progress),
        )
        .await
        .map_err(|stop| {
            let code = match stop {
                crate::text_completion::RequestStop::TimedOut => "PI_MODEL_JOB_TIMEOUT",
                crate::text_completion::RequestStop::Cancelled => "PI_MODEL_JOB_CANCELLED",
                crate::text_completion::RequestStop::TimeUnavailable => "PI_MODEL_JOB_PERMISSION",
            };
            Error::tool("eval", operation.admission_error(code.into()))
        })?;
        check()?;
        result
    }

    async fn run_prepared(
        self,
        judge: Judge,
        operation: Operation,
        base: usize,
        privacy: AuxiliaryPrivacySnapshot,
        deadline: Instant,
        progress: Option<JobProgress>,
    ) -> Result<ToolOutput> {
        match self {
            Self::Completion(prepared) => {
                let result = judge.run_prepared_completion(prepared).await;
                let receipt = attempt(base, operation, &result);
                match result {
                    Ok(value) => output(operation, value, vec![receipt], None, false, &privacy),
                    Err(error) => output(
                        operation,
                        Value::Null,
                        vec![receipt],
                        Some(operation.error(error)),
                        true,
                        &privacy,
                    ),
                }
            }
            Self::Judgment(prepared) => {
                let result = judge.run_prepared_judgment(prepared).await;
                let receipt = attempt(base, operation, &result);
                match result {
                    Ok(value) => output(operation, value, vec![receipt], None, false, &privacy),
                    Err(error) => output(
                        operation,
                        Value::Null,
                        vec![receipt],
                        Some(operation.error(error)),
                        true,
                        &privacy,
                    ),
                }
            }
            Self::Batch(items, concurrency, options) => {
                let progress = progress.ok_or_else(stopped)?;
                let mut batch = BatchProgress::new(&items, base, options);
                progress.publish(&batch, &privacy)?;
                let owner = AgentCx::for_current_or_request();
                let mut waiting: VecDeque<_> = (0..items.len()).collect();
                let mut active = FuturesUnordered::new();
                loop {
                    checkpoint(&owner, deadline)?;
                    let mut admitted = false;
                    while active.len() < concurrency {
                        let Some(index) = waiting.pop_front() else {
                            break;
                        };
                        progress.screened(batch.running(index))?;
                        admitted = true;
                        let judge = judge.clone();
                        let item_owner = owner.clone();
                        let (key, request) = items[index].clone();
                        active.push(async move {
                            let result = item_owner
                                .with_current(async {
                                    checkpoint(&item_owner, deadline)?;
                                    judge.run_prepared_judgment_attempt(request).await
                                })
                                .await;
                            (index, key, result)
                        });
                    }
                    if admitted {
                        // Futures have been queued but not polled. Publish the
                        // bounded wave before its first provider dispatch.
                        progress.publish(&batch, &privacy)?;
                    }
                    let Some((index, key, outcome)) = active.next().await else {
                        break;
                    };
                    checkpoint(&owner, deadline)?;
                    let retry = outcome.as_ref().is_err_and(|failure| failure.retryable)
                        && batch.attempts[index] <= options.retries;
                    let outcome = outcome.map_err(|failure| failure.error);
                    progress.screened(batch.completed(index, key, outcome, retry))?;
                    if retry {
                        // Untouched peers retain their place ahead of retries.
                        waiting.push_back(index);
                    }
                    // Commit each complete aggregate before replenishing the
                    // queue; cancellation retains only accepted observations.
                    progress.publish(&batch, &privacy)?;
                }
                progress.screened(batch.finish(&privacy))
            }
        }
    }
}

#[cfg(test)]
mod tests;
