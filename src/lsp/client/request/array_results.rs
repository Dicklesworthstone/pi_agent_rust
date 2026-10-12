//! Bounded streamed results for read-only symbol and navigation requests.
//!
//! The shared partial-result owner handles tokens, queue loss and cancellation.
//! This layer assembles array fragments without renewing the request deadline
//! or mistaking an empty terminal response for an empty search result.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;

use super::{
    LspClient, RequestBudget, WARMUP_EMPTY_RESULT_WINDOW, WARMUP_RETRY_CADENCE,
    is_empty_result, is_warmup_empty_retryable,
};
use crate::error::{Error, Result};

fn partial_error(message: &str) -> Error {
    Error::tool("lsp", format!("[LSP_PARTIAL_RESULT] {message}"))
}

/// Preserve nonstreamed response shapes for the operation-specific validator.
/// A definition may legitimately be one Location rather than an array. Once
/// any fragment arrives, however, every fragment must be an array and the
/// terminal result must be null or an empty array. Do not guess whether a
/// nonempty terminal replaces or appends to already streamed results.
fn assemble(result: Value, fragments: Vec<Value>, max_items: usize) -> Result<Value> {
    if fragments.is_empty() {
        return Ok(result);
    }
    if !result.is_null() && !result.as_array().is_some_and(Vec::is_empty) {
        return Err(partial_error(
            "streamed array results require a null or empty-array terminal response",
        ));
    }
    let mut items = Vec::new();
    for fragment in fragments {
        let Value::Array(mut batch) = fragment else {
            return Err(partial_error("array result fragment must be an array"));
        };
        if batch.len() > max_items.saturating_sub(items.len()) {
            return Err(partial_error(
                "streamed array exceeds the combined item limit",
            ));
        }
        // Move server-owned values; opaque symbol data and location metadata
        // retain their exact types and order for later validation/resolution.
        items.append(&mut batch);
    }
    Ok(Value::Array(items))
}

impl LspClient {
    /// Collect one bounded array-valued read operation before exposing it.
    ///
    /// The encoded-byte budget covers all fragments plus the terminal result.
    /// Operation-specific validators must still check every assembled member,
    /// including members omitted by the caller's display limit. With no
    /// fragments, the original response shape is preserved for those validators.
    /// Never use this for a command or an operation with remote side effects.
    pub(in crate::lsp) async fn call_with_array_partial_results(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        max_bytes: usize,
        max_items: usize,
    ) -> Result<Value> {
        let budget = RequestBudget::new(timeout);
        loop {
            let (result, fragments) = self
                .call_with_partial_results(method, params.clone(), &budget, max_bytes)
                .await?;
            let result = assemble(result, fragments, max_items)?;
            budget.remaining().map_err(Error::from)?;
            // Preserve the ordinary call path's indexing warmup policy. Test
            // the ASSEMBLED response, not the deliberately empty terminal.
            // A retry creates a new collection/token but keeps this deadline.
            let empty_during_warmup = is_empty_result(&result)
                && is_warmup_empty_retryable(method)
                && !self.quiescent.load(Ordering::SeqCst)
                && self.connected_at.elapsed() < WARMUP_EMPTY_RESULT_WINDOW;
            if !empty_during_warmup
                || budget.remaining().map_err(Error::from)? < WARMUP_RETRY_CADENCE * 2
            {
                return Ok(result);
            }
            budget
                .pause(WARMUP_RETRY_CADENCE)
                .await
                .map_err(Error::from)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nonstreamed_shapes_remain_owned_by_the_operation_validator() {
        for result in [
            Value::Null,
            json!([]),
            json!([1]),
            json!({"uri":"untitled:x"}),
        ] {
            assert_eq!(assemble(result.clone(), Vec::new(), 4).unwrap(), result);
        }
    }

    #[test]
    fn fragments_preserve_order_duplicates_and_opaque_symbol_data() {
        let first = json!({"name":"界", "data":{"ticket":3,"value":[true,null,7]}});
        let second = json!({"name":"same", "data":"$(not executed)"});
        for terminal in [Value::Null, json!([])] {
            assert_eq!(
                assemble(
                    terminal,
                    vec![json!([first]), json!([]), json!([second, first])],
                    3,
                )
                .unwrap(),
                json!([first, second, first]),
            );
        }
    }

    #[test]
    fn progress_requires_a_successful_empty_terminal_not_a_replacement() {
        for terminal in [json!([2]), json!({}), json!(false), json!({"items":[]})] {
            assert!(assemble(terminal, vec![json!([1])], 8).is_err());
        }
    }

    #[test]
    fn malformed_fragments_are_not_silently_ignored_even_at_the_tail() {
        for fragment in [Value::Null, json!({"items":[]}), json!(false), json!(42)] {
            assert!(assemble(Value::Null, vec![json!([1]), fragment], 8).is_err());
        }
    }

    #[test]
    fn item_limit_is_cumulative_across_fragment_boundaries() {
        assert!(assemble(Value::Null, vec![json!([1,2]), json!([3,4])], 3).is_err());
        assert_eq!(
            assemble(Value::Null, vec![json!([]), json!([])], 0).unwrap(),
            json!([]),
        );
        assert!(assemble(Value::Null, vec![json!([1])], 0).is_err());
    }
}
