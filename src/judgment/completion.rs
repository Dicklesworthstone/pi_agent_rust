//! Stateless text completion through the existing bound auxiliary model.
//!
//! Prompt and system are data, not selectors for credentials, tools or history.
//! A completion uses the same privacy floor and terminal collector as a judgment.

use super::{Judge, Protect};
use crate::error::{Error, Result};
use serde_json::Value;

mod schema;

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_SYSTEM_BYTES: usize = 16 * 1024;
const DEFAULT_SYSTEM: &str = "Answer the supplied request. Return the answer as text. Do not call tools.";

fn invalid() -> Error {
    Error::tool("eval", "PI_COMPLETION_INPUT: expected prompt, optional system, schema and max_tokens (1-4096); prompt/system must be nonempty text of at most 64/16 KiB")
}

/// Validated input for a one-shot, tool-free auxiliary completion. No model,
/// endpoint, credential, transcript or execution controls can come from input.
pub struct CompletionRequest {
    prompt: String,
    system: String,
    max_tokens: Option<u32>,
    schema: Option<schema::OutputSchema>,
}

impl CompletionRequest {
    pub fn parse(input: Value) -> Result<Self> {
        let fields = input.as_object().ok_or_else(invalid)?;
        if fields.keys().any(|key| !matches!(key.as_str(), "prompt" | "system" | "max_tokens" | "schema")) {
            return Err(invalid());
        }
        let prompt = fields.get("prompt").and_then(Value::as_str).ok_or_else(invalid)?;
        let system = match fields.get("system") {
            None => DEFAULT_SYSTEM,
            Some(value) => value.as_str().ok_or_else(invalid)?,
        };
        validate_text(prompt, system)?;
        let max_tokens = fields.get("max_tokens").map(|value| {
            value.as_u64().filter(|value| (1..=4096).contains(value))
                .and_then(|value| u32::try_from(value).ok()).ok_or_else(invalid)
        }).transpose()?;
        // Compile before any attempt reservation, credential resolution or
        // provider I/O. A schema cannot grant a reference retriever authority.
        let schema = fields.get("schema").map(schema::OutputSchema::parse).transpose()?;
        // Check all borrowed values before cloning any guest-controlled text.
        if let Some(schema) = &schema {
            schema.envelope(prompt)?;
        }
        Ok(Self { prompt: prompt.to_owned(), system: system.to_owned(), max_tokens, schema })
    }
}

fn validate_text(prompt: &str, system: &str) -> Result<()> {
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES
        || system.trim().is_empty() || system.len() > MAX_SYSTEM_BYTES
    {
        return Err(invalid());
    }
    Ok(())
}

impl Judge {
    /// Use this same explicit provider binding for text or schema-validated
    /// JSON. No conversation is replayed and no tool is advertised or executed.
    pub async fn complete(&self, request: CompletionRequest) -> Result<Value> {
        self.complete_protected(request, None).await
    }

    pub(crate) async fn complete_protected(
        &self,
        request: CompletionRequest,
        protect: Option<&Protect<'_>>,
    ) -> Result<Value> {
        let (system, prompt) = match &request.schema {
            Some(schema) => {
                // Screen prompt and schema together: annotations and example
                // values can contain secrets too. Never validate against a
                // redacted/weakened contract or send the unscreened original.
                let envelope = schema.envelope(&request.prompt)?;
                let [system, envelope] = self.screen(&request.system, &envelope, protect)?;
                let prompt = schema.screened_prompt(&envelope)?;
                validate_text(&prompt, &system)?;
                (schema::json_system(system), envelope)
            }
            None => {
                let [system, prompt] = self.screen(&request.system, &request.prompt, protect)?;
                validate_text(&prompt, &system)?;
                (system, prompt)
            }
        };
        let (text, usage) = self.request_text(system, prompt, request.max_tokens).await?;
        let data = request.schema.as_ref().map(|schema| schema.answer(&text)).transpose()?;
        let mut reply = self.reply("pi.completion.v1", "text", Value::String(text), usage);
        if let Some(data) = data {
            // Explicit JSON null is valid data. Absence means no schema was
            // requested; neither malformed output nor failure yields null.
            reply["data"] = data;
            reply["schemaValid"] = Value::Bool(true);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_text_and_explicit_limits_are_preserved() {
        let input = json!({"prompt":"  literal 雪\n{previous}\0  ","system":"  answer literally  ","max_tokens":7});
        let request = CompletionRequest::parse(input.clone()).unwrap();
        assert_eq!(request.prompt, input["prompt"].as_str().unwrap());
        assert_eq!(request.system, input["system"].as_str().unwrap());
        assert_eq!(request.max_tokens, Some(7));
        let default = CompletionRequest::parse(json!({"prompt":"work"})).unwrap();
        assert_eq!(default.system, DEFAULT_SYSTEM);
        assert_eq!(default.max_tokens, None);
    }

    #[test]
    fn invalid_fields_and_guest_authority_are_rejected_before_execution() {
        for input in [Value::Null, json!([]), json!({}), json!({"prompt":" "}),
            json!({"prompt":null}), json!({"prompt":"x","system":null}),
            json!({"prompt":"x","system":""}),
            json!({"prompt":"x","model":"different"}),
            json!({"prompt":"x","api_key":"CANARY"}),
            json!({"prompt":"x","tools":["bash"]}),
            json!({"prompt":"x","messages":[]}),
        ] {
            let error = CompletionRequest::parse(input).err().unwrap().to_string();
            assert!(error.contains("PI_COMPLETION_INPUT"));
            assert!(!error.contains("CANARY"));
        }
        for value in [Value::Null,json!(0),json!(-1),json!(4097),json!(1.5),json!("5"),json!(true)] {
            assert!(CompletionRequest::parse(json!({"prompt":"x","max_tokens":value})).is_err());
        }
    }

    #[test]
    fn utf8_budgets_are_bytes_and_apply_to_the_complete_field() {
        assert!(CompletionRequest::parse(json!({"prompt":"x".repeat(MAX_PROMPT_BYTES),"system":"x".repeat(MAX_SYSTEM_BYTES)})).is_ok());
        assert!(CompletionRequest::parse(json!({"prompt":"x".repeat(MAX_PROMPT_BYTES+1)})).is_err());
        assert!(CompletionRequest::parse(json!({"prompt":"雪".repeat(MAX_PROMPT_BYTES/3+1)})).is_err());
        assert!(CompletionRequest::parse(json!({"prompt":"x","system":"雪".repeat(MAX_SYSTEM_BYTES/3+1)})).is_err());
    }
}
