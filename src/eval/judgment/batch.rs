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

use super::{AgentCx, Invocation, JudgmentRequest, MAX_CALLS, images, public_error};

pub(super) const BRIDGE_NAME: &str = "__pi_judge_batch";
const MAX_CONCURRENCY: usize = 8;
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const INVALID: &str = "PI_JUDGE_BATCH_INPUT: requires 1-16 states, shared questions, optional concurrency from 1 to 8 and an optional images map keyed by exact state keys or array indices";
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

pub(super) fn encoded_size(value: &impl Serialize, limit: usize) -> BatchResult<usize> {
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| LIMIT.to_string())?;
    Ok(counter.bytes)
}

pub(super) struct Item {
    pub(super) key: Value,
    pub(super) request: JudgmentRequest,
    pub(super) images: images::Sources,
}

pub(super) struct Plan {
    pub(super) items: Vec<Item>,
    pub(super) concurrency: usize,
}

impl Plan {
    pub(super) fn parse(input: &Value) -> BatchResult<Self> {
        let fields = input.as_object().filter(|fields| {
            fields.contains_key("states") && fields.contains_key("questions")
                && fields.keys().all(|key| matches!(key.as_str(), "states" | "questions" | "concurrency" | "images"))
        }).ok_or_else(|| INVALID.to_string())?;
        // Images have their own bounded budget. Count the original text wire
        // shape without copying or serializing base64 into the rubric budget.
        let text_fields: std::collections::BTreeMap<_, _> = fields.iter()
            .filter(|(key, _)| key.as_str() != "images")
            .collect();
        encoded_size(&text_fields, MAX_INPUT_BYTES)?;
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
        let image_map = fields.get("images").map(|value| {
            value.as_object().ok_or_else(|| INVALID.to_string())
        }).transpose()?;
        let image_key = |key: &Value| {
            key.as_str().map_or_else(|| key.to_string(), str::to_owned)
        };
        if image_map.is_some_and(|map| map.keys().any(|key| {
            !selected.iter().any(|(selected_key, _)| image_key(selected_key) == *key)
        })) {
            return Err(INVALID.to_string());
        }
        #[derive(Serialize)]
        struct BorrowedRequest<'a> {
            state: &'a Value,
            questions: &'a Value,
        }
        let questions = &fields["questions"];
        let mut remaining = MAX_INPUT_BYTES;
        let mut image_bytes = 0_usize;
        let mut items = Vec::with_capacity(selected.len());
        for (key, state) in selected {
            let borrowed = BorrowedRequest { state, questions };
            remaining -= encoded_size(&borrowed, remaining)?;
            // Reuse the single text validator, including its 128 KiB bound.
            // Images remain separate from text and its privacy projection.
            let request = JudgmentRequest::parse(json!({"state":state,"questions":questions}))
                .map_err(public_error)?;
            let images = images::Sources::parse(image_map.and_then(|map| map.get(&image_key(&key))))
                .map_err(public_error)?;
            image_bytes = image_bytes.checked_add(images.inline_bytes())
                .filter(|bytes| *bytes <= images::MAX_BATCH_IMAGE_BYTES)
                .ok_or_else(|| public_error(images::limit()))?;
            items.push(Item { key, request, images });
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
    pub(super) async fn evaluate_batch(&mut self, cwd: &Path, mut input: Value) -> BatchResult<String> {
        let selection = self.request_selection(&mut input).map_err(public_error)?;
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
                "selectedModel":selection, "status":"not_started",
                "usage":null, "usageKnown":false,
            }));
        }
        let judge = match self.bound_judge(cwd, selection.as_ref()).await {
            Ok(judge) => judge,
            Err(error) => {
                let error = public_error(error);
                for receipt in &mut self.receipts[base..] {
                    receipt["admissionError"] = json!(error);
                }
                return Err(error);
            }
        };
        let concurrency = plan.concurrency;
        // Prepare every image before the first provider request. An invalid
        // later path or an aggregate byte overflow cannot send earlier states.
        // Reads still use the original cell's host, policy, owner and watchdog.
        let prepared = async {
            check_owner(&owner)?;
            judge.check_image_support(plan.items.iter().any(|item| !item.images.is_empty()))
                .map_err(public_error)?;
            self.check_image_reads(plan.items.iter().map(|item| item.images.path_count()).sum())
                .map_err(public_error)?;
            let mut bytes = 0_usize;
            let mut prepared = Vec::with_capacity(count);
            for item in plan.items {
                check_owner(&owner)?;
                let request = self.attach_images(item.request, item.images).await.map_err(public_error)?;
                bytes = bytes.checked_add(request.image_bytes())
                    .filter(|bytes| *bytes <= images::MAX_BATCH_IMAGE_BYTES)
                    .ok_or_else(|| public_error(images::limit()))?;
                prepared.push((item.key, request));
            }
            check_owner(&owner)?;
            Ok::<_, String>(prepared)
        }.await;
        let items = match prepared {
            Ok(items) => items,
            Err(error) => {
                for receipt in &mut self.receipts[base..] {
                    receipt["admissionError"] = json!(error);
                }
                return Err(error);
            }
        };
        let keys: Vec<_> = items.iter().map(|(key, _)| key.clone()).collect();
        let mut waiting = items.into_iter().enumerate();
        let mut active = FuturesUnordered::new();
        let mut results = vec![Value::Null; count];
        let mut succeeded = 0_usize;
        loop {
            while active.len() < concurrency {
                check_owner(&owner)?;
                let Some((index, (_, request))) = waiting.next() else { break; };
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
                        judge.evaluate_protected(request, protect),
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
