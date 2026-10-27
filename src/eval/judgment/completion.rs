//! One-shot completions share the cell's binding, receipts and attempt ceiling.

use super::{Invocation, MAX_CALLS};
use crate::error::Error;
use crate::judgment::CompletionRequest;
use serde_json::{Value, json};
use std::path::Path;

pub(super) const BRIDGE_NAME: &str = "__pi_complete";

impl Invocation<'_> {
    pub(super) async fn complete(
        &mut self,
        cwd: &Path,
        mut input: Value,
    ) -> Result<String, String> {
        let selection = self.request_selection(&mut input).map_err(public_error)?;
        let Value::Object(mut fields) = input else {
            return Err("PI_COMPLETION_INPUT: expected completion arguments".into());
        };
        let raw_images = fields.remove("images");
        // Compile the schema and validate every descriptor before any read,
        // credential resolution, attempt reservation or provider admission.
        let request = CompletionRequest::parse(Value::Object(fields)).map_err(public_error)?;
        let images = super::images::Sources::parse(raw_images.as_ref()).map_err(public_error)?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err(
                "PI_COMPLETION_UNBOUND: supply this cell's judge_model or bind a Judge in the host"
                    .into(),
            );
        }
        if self.receipts.len() >= MAX_CALLS {
            return Err(
                "PI_COMPLETION_LIMIT: completions and judgments share 16 attempts per eval cell"
                    .into(),
            );
        }
        let index = self.receipts.len();
        self.receipts.push(json!({
            "attempt":index+1, "operation":"completion", "selectedModel":selection,
            "status":"incomplete", "usage":null, "usageKnown":false,
        }));
        // The pending generation belongs to this bridge await. No task or
        // interpreter handle can carry the borrowed privacy callback elsewhere.
        let outcome = async {
            let judge = self.bound_judge(cwd, selection.as_ref()).await?;
            judge.check_image_support(!images.is_empty())?;
            self.check_image_reads(images.path_count())?;
            let request = request.with_images(self.load_images(images).await?)?;
            judge.complete_protected(request, self.protect).await
        }
        .await;
        match outcome {
            Ok(reply) => {
                self.receipts[index] = json!({
                    "attempt":index+1, "operation":"completion", "status":"completed",
                    "selectedModel":selection,
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

#[allow(clippy::needless_pass_by_value)] // `map_err` adapter at every call site
pub(super) fn public_error(error: Error) -> String {
    if matches!(&error, Error::CostCap { .. }) {
        return format!("cost_cap: {error}");
    }
    let text = error.to_string();
    for code in [
        "PI_SECRET_BLOCK",
        "PI_AUXILIARY_INPUT_LIMIT",
        "PI_AUXILIARY_KEY_COLLISION",
        "PI_COMPLETION_INPUT",
        "PI_COMPLETION_SCHEMA",
        "PI_COMPLETION_PRIVACY",
        "PI_COMPLETION_RESPONSE",
    ] {
        if text.contains(code) {
            return format!("{code}: completion failed; no answer");
        }
    }
    for suffix in [
        "INPUT",
        "BINDING",
        "PRIVACY",
        "PERMISSION",
        "CANCELLED",
        "TIMEOUT",
        "RESPONSE",
        "PROVIDER",
        "IMAGE_UNSUPPORTED",
        "IMAGE_INPUT",
        "IMAGE_LIMIT",
        "IMAGE_READ",
        "IMAGE_HOST",
        "IMAGE_CALL_LIMIT",
    ] {
        if text.contains(&format!("PI_JUDGE_{suffix}")) {
            return format!("PI_COMPLETION_{suffix}: completion failed; no answer");
        }
    }
    "PI_COMPLETION_FAILED: completion failed; no answer".into()
}

#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod tests;
