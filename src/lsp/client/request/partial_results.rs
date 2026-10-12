//! Request-owned LSP partial results. A progress token is not a completion.
//!
//! Notification polling may run on any caller, so collect under the client's
//! shared router rather than competing with diagnostics_snapshot for the wire.
//! Only an observed successful response authorizes delivery. Loss, overflow,
//! timeout, cancellation and abandoned attempts discard the whole collection.

use std::io::{self, Write};

use futures::future::{Either, select};
use serde_json::Value;

use super::super::LspClient;
use super::{RequestBudget, WAIT_TICK, WARMUP_RETRY_CADENCE, is_retryable_request_error};
use crate::error::{Error, Result};

const MAX_PARTS: usize = 2048;

fn partial_error(message: &str) -> Error {
    Error::tool("lsp", format!("[LSP_PARTIAL_RESULT] {message}"))
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("partial result byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Collection {
    token: String,
    values: Vec<Value>,
    remaining_bytes: usize,
    failure: Option<&'static str>,
}

impl Collection {
    fn fail(&mut self, reason: &'static str) {
        self.failure.get_or_insert(reason);
        self.values.clear();
    }

    fn accept(&mut self, mut params: Value) {
        if params.get("token").and_then(Value::as_str) != Some(self.token.as_str())
            || self.failure.is_some()
        {
            return;
        }
        let Some(value) = params.get_mut("value") else {
            self.fail("matching progress notification has no value");
            return;
        };
        if self.values.len() >= MAX_PARTS {
            self.fail("partial result has too many fragments");
            return;
        }
        // Count encoded bytes without constructing another serialized copy.
        // Move only an admitted value into the bounded retained working set.
        let mut budget = ByteBudget(self.remaining_bytes);
        if serde_json::to_writer(&mut budget, value).is_err() {
            self.fail("partial result exceeded the cumulative byte limit");
            return;
        }
        self.remaining_bytes = budget.0;
        self.values.push(value.take());
    }
}

/// There is at most one registered collection because the request lane is
/// acquired BEFORE registration and retained until the collection is retired.
#[derive(Default)]
pub(in crate::lsp::client) struct PartialResults {
    active: Option<Collection>,
}

impl PartialResults {
    pub(in crate::lsp::client) fn accept(&mut self, params: Value) {
        if let Some(active) = &mut self.active {
            active.accept(params);
        }
    }
}

struct PartialOwner<'a> {
    client: &'a LspClient,
    token: String,
    dropped_before: u64,
}

impl<'a> PartialOwner<'a> {
    fn register(client: &'a LspClient, max_bytes: usize) -> Result<Self> {
        client.poll_notifications();
        let dropped_before = client.rpc.dropped_notifications();
        if dropped_before == u64::MAX {
            return Err(partial_error("notification loss counter exhausted"));
        }
        let token = format!("pi-partial-{}", uuid::Uuid::new_v4());
        let mut inbox = LspClient::lock(&client.partial_results);
        if inbox.active.is_some() {
            return Err(partial_error(
                "another partial result still owns the request lane",
            ));
        }
        inbox.active = Some(Collection {
            token: token.clone(),
            values: Vec::new(),
            remaining_bytes: max_bytes,
            failure: None,
        });
        drop(inbox);
        Ok(Self {
            client,
            token,
            dropped_before,
        })
    }

    fn check(&self) -> Result<()> {
        // Any queue loss may have been one of our fragments. A subsequent
        // successful response cannot prove the missing part was harmless.
        if self.client.rpc.dropped_notifications() != self.dropped_before {
            return Err(partial_error("notifications were lost during the request"));
        }
        let inbox = LspClient::lock(&self.client.partial_results);
        let active = inbox
            .active
            .as_ref()
            .filter(|active| active.token == self.token)
            .ok_or_else(|| partial_error("partial result ownership was retired"))?;
        if let Some(reason) = active.failure {
            return Err(partial_error(reason));
        }
        Ok(())
    }

    fn finish(self, result: &Value) -> Result<Vec<Value>> {
        // Retire and inspect the SAME owned collection. Another snapshot
        // caller must not fail/clear it between a check and a later take,
        // turning a rejected stream into an empty successful collection.
        let active = {
            let mut inbox = LspClient::lock(&self.client.partial_results);
            if inbox
                .active
                .as_ref()
                .is_none_or(|active| active.token != self.token)
            {
                return Err(partial_error("partial result ownership was retired"));
            }
            inbox
                .active
                .take()
                .ok_or_else(|| partial_error("partial result ownership was retired"))?
        };
        if self.client.rpc.dropped_notifications() != self.dropped_before {
            return Err(partial_error("notifications were lost during the request"));
        }
        if let Some(reason) = active.failure {
            return Err(partial_error(reason));
        }
        // The terminal response shares the fragments' byte budget. Do not
        // allow N bounded fragments plus another independently bounded result.
        serde_json::to_writer(&mut ByteBudget(active.remaining_bytes), result).map_err(|_| {
            partial_error("response and fragments exceeded the cumulative byte limit")
        })?;
        Ok(active.values)
    }
}

impl Drop for PartialOwner<'_> {
    fn drop(&mut self) {
        let mut inbox = LspClient::lock(&self.client.partial_results);
        if inbox
            .active
            .as_ref()
            .is_some_and(|active| active.token == self.token)
        {
            inbox.active = None;
        }
    }
}

impl LspClient {
    /// Collect fragments for one attempt only. Every retry obtains a new
    /// token and discards prior data; the original deadline is never renewed.
    pub(super) async fn call_with_partial_results(
        &self,
        method: &str,
        params: Value,
        budget: &RequestBudget,
        max_bytes: usize,
    ) -> Result<(Value, Vec<Value>)> {
        loop {
            let lane = budget
                .acquire(&self.request_lane)
                .await
                .map_err(Error::from)?;
            budget.remaining().map_err(Error::from)?;
            let partials = PartialOwner::register(self, max_bytes)?;
            let mut attempt_params = params.clone();
            let object = attempt_params.as_object_mut().ok_or_else(|| {
                partial_error("partial-result request parameters must be an object")
            })?;
            object.insert(
                "partialResultToken".to_string(),
                Value::String(partials.token.clone()),
            );
            // Declare the request after its owners so Drop cancels its pending
            // transport slot before retiring the inbox and releasing the lane.
            let mut call = Box::pin(self.call_once_in_lane(method, attempt_params, budget));
            let attempt = loop {
                budget.remaining().map_err(Error::from)?;
                self.poll_notifications();
                partials.check()?;
                let pause = Box::pin(budget.pause(WAIT_TICK));
                match select(call.as_mut(), pause).await {
                    Either::Left((result, _)) => break result,
                    Either::Right((result, _)) => result.map_err(Error::from)?,
                }
            };
            drop(call);
            // The stdout pump queues preceding notifications before posting
            // the response. Drain AFTER observing that response, even when it
            // was already buffered at the first poll. Shared polling is ordered.
            self.poll_notifications();
            partials.check()?;
            match attempt {
                Ok(result) => {
                    let fragments = partials.finish(&result)?;
                    budget.remaining().map_err(Error::from)?;
                    return Ok((result, fragments));
                }
                Err(error) => {
                    let retry = is_retryable_request_error(method, &error);
                    drop(partials);
                    drop(lane);
                    if !retry || budget.remaining().map_err(Error::from)? < WARMUP_RETRY_CADENCE * 2
                    {
                        return Err(Error::from(error));
                    }
                    budget
                        .pause(WARMUP_RETRY_CADENCE)
                        .await
                        .map_err(Error::from)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod wire_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collection(bytes: usize) -> Collection {
        Collection {
            token: "owned".to_string(),
            values: Vec::new(),
            remaining_bytes: bytes,
            failure: None,
        }
    }

    #[test]
    fn unrelated_work_done_and_old_attempt_tokens_cannot_enter_the_collection() {
        let mut active = collection(100);
        for token in [json!("old"), json!("work-done"), json!(12), Value::Null] {
            active.accept(json!({"token":token,"value":{"items":["ignored"]}}));
        }
        assert!(active.values.is_empty());
        assert_eq!(active.remaining_bytes, 100);
        active.accept(json!({"token":"owned","value":{"items":[]}}));
        assert_eq!(active.values, vec![json!({"items":[]})]);
    }

    #[test]
    fn fragments_spend_one_utf8_byte_budget_and_failure_discards_prior_data() {
        let value = json!({"items":["日本語"]});
        let size = serde_json::to_vec(&value).unwrap().len();
        let mut active = collection(size * 2);
        active.accept(json!({"token":"owned","value":value}));
        active.accept(json!({"token":"owned","value":value}));
        assert_eq!(active.values.len(), 2);
        assert_eq!(active.remaining_bytes, 0);
        active.accept(json!({"token":"owned","value":{"items":[]}}));
        assert!(active.failure.is_some());
        assert!(active.values.is_empty());
        active.accept(json!({"token":"owned","value":null}));
        assert!(
            active.values.is_empty(),
            "a failed stream cannot repopulate its result"
        );
    }

    #[test]
    fn empty_fragments_are_count_bounded_and_missing_values_fail_closed() {
        let mut active = collection(1024 * 1024);
        for _ in 0..MAX_PARTS {
            active.accept(json!({"token":"owned","value":{"items":[]}}));
        }
        assert!(active.failure.is_none());
        active.accept(json!({"token":"owned","value":{"items":[]}}));
        assert_eq!(
            active.failure,
            Some("partial result has too many fragments")
        );
        assert!(active.values.is_empty());
        let mut active = collection(100);
        active.accept(json!({"token":"owned"}));
        assert_eq!(
            active.failure,
            Some("matching progress notification has no value")
        );
    }
}
