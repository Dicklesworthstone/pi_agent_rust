//! One-shot completions share the cell's binding, receipts and attempt ceiling.

use super::{Invocation, MAX_CALLS};
use crate::error::Error;
use crate::judgment::CompletionRequest;
use serde_json::{Value, json};
use std::path::Path;

pub(super) const BRIDGE_NAME: &str = "__pi_complete";

impl Invocation<'_> {
    pub(super) async fn complete(&mut self, cwd: &Path, input: Value) -> Result<String, String> {
        let request = CompletionRequest::parse(input).map_err(public_error)?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err("PI_COMPLETION_UNBOUND: supply this cell's judge_model or bind a Judge in the host".into());
        }
        if self.receipts.len() >= MAX_CALLS {
            return Err("PI_COMPLETION_LIMIT: completions and judgments share 16 attempts per eval cell".into());
        }
        let index = self.receipts.len();
        self.receipts.push(json!({
            "attempt":index+1, "operation":"completion", "selectedModel":self.selection,
            "status":"incomplete", "usage":null, "usageKnown":false,
        }));
        // The pending generation belongs to this bridge await. No task or
        // interpreter handle can carry the borrowed privacy callback elsewhere.
        let outcome = async {
            self.bound_judge(cwd).await?
                .complete_protected(request, self.protect).await
        }.await;
        match outcome {
            Ok(reply) => {
                self.receipts[index] = json!({
                    "attempt":index+1, "operation":"completion", "status":"completed",
                    "provider":reply["provider"], "modelId":reply["modelId"],
                    "api":reply["api"], "usage":reply["usage"],
                    "usageKnown":!reply["usage"].is_null(),
                });
                serde_json::to_string(&reply)
                    .map_err(|_| "PI_COMPLETION_RESPONSE: could not encode answer".to_string())
            }
            Err(error) => {
                let error = public_error(error);
                self.receipts[index]["status"] = json!("failed");
                self.receipts[index]["error"] = json!(error);
                Err(error)
            }
        }
    }
}

fn public_error(error: Error) -> String {
    let text = error.to_string();
    for code in ["PI_SECRET_BLOCK", "PI_AUXILIARY_INPUT_LIMIT", "PI_AUXILIARY_KEY_COLLISION", "PI_COMPLETION_INPUT"] {
        if text.contains(code) { return format!("{code}: completion failed; no answer"); }
    }
    for suffix in ["BINDING", "PRIVACY", "PERMISSION", "CANCELLED", "TIMEOUT", "RESPONSE", "PROVIDER"] {
        if text.contains(&format!("PI_JUDGE_{suffix}")) {
            return format!("PI_COMPLETION_{suffix}: completion failed; no answer");
        }
    }
    "PI_COMPLETION_FAILED: completion failed; no answer".into()
}

#[cfg(test)]
mod tests;
