//! Cell-owned batches of ordinary typed judgments, not detached model jobs.
//!
//! Validate the whole batch before resolving credentials or admitting work.
//! One bounded set of in-flight futures borrows the original privacy callback;
//! dropping the cell drops them all. Receipts remain outside guest output.

use std::io::{self, Write};
use std::path::Path;

use futures::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};

use super::{AgentCx, Invocation, JudgmentRequest, MAX_CALLS, public_error};

pub(super) const BRIDGE_NAME: &str = "__pi_judge_batch";
const MAX_CONCURRENCY: usize = 8;
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const INVALID: &str = "PI_JUDGE_BATCH_INPUT: requires 1-16 states, shared questions and optional concurrency from 1 to 8; states may be an array or a keyed object";
const LIMIT: &str = "PI_JUDGE_BATCH_LIMIT: batch input or expanded requests exceed 256 KiB";
const CALL_LIMIT: &str = "PI_JUDGE_LIMIT: single and batch judgments share 16 reserved attempts per eval cell";

type BatchResult<T> = std::result::Result<T, String>;

struct Counter {
    bytes: usize,
    limit: usize,
}

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("judgment batch byte limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn encoded_size(value: &impl Serialize, limit: usize) -> BatchResult<usize> {
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| LIMIT.to_string())?;
    Ok(counter.bytes)
}

struct Item {
    key: Value,
    request: JudgmentRequest,
}

struct Plan {
    items: Vec<Item>,
    concurrency: usize,
}

impl Plan {
    fn parse(input: &Value) -> BatchResult<Self> {
        encoded_size(input, MAX_INPUT_BYTES)?;
        let fields = input.as_object().filter(|fields| {
            fields.contains_key("states") && fields.contains_key("questions")
                && fields.keys().all(|key| matches!(key.as_str(), "states" | "questions" | "concurrency"))
        }).ok_or_else(|| INVALID.to_string())?;
        let concurrency = match fields.get("concurrency") {
            None => DEFAULT_CONCURRENCY,
            Some(value) => value.as_u64()
                .filter(|value| (1..=MAX_CONCURRENCY as u64).contains(value))
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| INVALID.to_string())?,
        };
        // Bound the collection BEFORE making keys or duplicating the shared
        // rubric. Only array indices and literal object keys identify results.
        let selected: Vec<(Value, &Value)> = match &fields["states"] {
            Value::Array(states) if (1..=MAX_CALLS).contains(&states.len()) => {
                states.iter().enumerate().map(|(index, state)| (json!(index), state)).collect()
            }
            Value::Object(states) if (1..=MAX_CALLS).contains(&states.len()) => {
                if states.keys().any(|key| key.is_empty() || key.len() > 128 || key.chars().any(char::is_control)) {
                    return Err(INVALID.to_string());
                }
                states.iter().map(|(key, state)| (json!(key), state)).collect()
            }
            _ => return Err(INVALID.to_string()),
        };
        #[derive(Serialize)]
        struct BorrowedRequest<'a> {
            state: &'a Value,
            questions: &'a Value,
        }
        let questions = &fields["questions"];
        let mut remaining = MAX_INPUT_BYTES;
        let mut items = Vec::with_capacity(selected.len());
        for (key, state) in selected {
            let borrowed = BorrowedRequest { state, questions };
            remaining -= encoded_size(&borrowed, remaining)?;
            // Reuse the actual single-request validator, including its 128 KiB
            // bound and refusal of images, empty text, bad rubrics or coercion.
            let request = JudgmentRequest::parse(json!({"state":state,"questions":questions}))
                .map_err(public_error)?;
            items.push(Item { key, request });
        }
        Ok(Self { items, concurrency })
    }
}

fn check_owner(owner: &AgentCx) -> BatchResult<()> {
    owner.checkpoint().map_err(|_| "PI_JUDGE_CANCELLED: judgment batch cancelled".to_string())?;
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err("PI_JUDGE_PERMISSION: judgment batch requires I/O and timer capabilities".into());
    }
    Ok(())
}

impl Invocation<'_> {
    pub(super) async fn evaluate_batch(&mut self, cwd: &Path, input: Value) -> BatchResult<String> {
        let plan = Plan::parse(&input)?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err("PI_JUDGE_UNBOUND: supply this cell's judge_model or bind a Judge in the host".into());
        }
        let count = plan.items.len();
        if count > MAX_CALLS.saturating_sub(self.receipts.len()) {
            return Err(CALL_LIMIT.into());
        }
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        let base = self.receipts.len();
        // Reserve all slots before the first await. Queued slots are NOT
        // provider calls. Keeping this distinction also covers a dropped cell
        // during credential resolution or while an earlier item is running.
        for index in 0..count {
            self.receipts.push(json!({
                "attempt":base + index + 1, "batch":base + 1, "item":index,
                "selectedModel":self.selection, "status":"not_started",
                "usage":null, "usageKnown":false,
            }));
        }
        let judge = match self.bound_judge(cwd).await {
            Ok(judge) => judge,
            Err(error) => {
                let error = public_error(error);
                for receipt in &mut self.receipts[base..] {
                    receipt["admissionError"] = json!(error);
                }
                return Err(error);
            }
        };
        check_owner(&owner)?;
        let keys: Vec<_> = plan.items.iter().map(|item| item.key.clone()).collect();
        let mut waiting = plan.items.into_iter().enumerate();
        let mut active = FuturesUnordered::new();
        let mut results = vec![Value::Null; count];
        let mut succeeded = 0_usize;
        loop {
            while active.len() < plan.concurrency {
                check_owner(&owner)?;
                let Some((index, item)) = waiting.next() else { break; };
                self.receipts[base + index]["status"] = json!("incomplete");
                let judge = judge.clone();
                let item_owner = owner.clone();
                let protect = self.protect;
                // No spawned task or retained handle. The same native Judge
                // still owns privacy, tool-free generation, answer validation
                // and its per-request ceiling. The enclosing cell watchdog is
                // NOT paused or refreshed while this future is pending.
                active.push(async move {
                    let outcome = item_owner.with_current(
                        judge.evaluate_protected(item.request, protect),
                    ).await;
                    (index, outcome)
                });
            }
            let Some((index, outcome)) = active.next().await else { break; };
            let receipt = &mut self.receipts[base + index];
            match outcome {
                Ok(verdict) => {
                    succeeded += 1;
                    receipt["status"] = json!("completed");
                    for field in ["provider", "modelId", "api", "usage"] {
                        receipt[field] = verdict[field].clone();
                    }
                    receipt["usageKnown"] = json!(!verdict["usage"].is_null());
                    results[index] = json!({"key":keys[index],"ok":true,"verdict":verdict});
                }
                Err(error) => {
                    let error = public_error(error);
                    receipt["status"] = json!("failed");
                    receipt["error"] = json!(error);
                    results[index] = json!({"key":keys[index],"ok":false,"error":error});
                }
            }
            // Completed evidence is retained before observing cancellation.
            // A cancellation never admits queued work or returns a partial
            // collection as a completed batch; dropping here closes active
            // provider futures without replay or a compensating request.
            check_owner(&owner)?;
        }
        let result = json!({
            "schema":"pi.eval.judgment-batch.v1", "batch":base + 1,
            "total":count, "succeeded":succeeded, "failed":count - succeeded,
            "allOk":succeeded == count, "items":results,
        });
        encoded_size(&result, MAX_OUTPUT_BYTES).map_err(|_| {
            "PI_JUDGE_BATCH_OUTPUT_LIMIT: completed batch exceeds 1 MiB; receipts remain available".to_string()
        })?;
        serde_json::to_string(&result)
            .map_err(|_| "PI_JUDGE_RESPONSE: could not encode judgment batch".to_string())
    }
}

#[cfg(test)]
mod tests;
