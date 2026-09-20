//! Decode native text/thought parts without untagged-enum fallthrough.
//!
//! A malformed signed thought must never become ordinary answer text merely
//! because a later enum variant accepts its `text` field. Unknown future part
//! kinds remain opaque, but known payloads and their signatures are validated.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::{GeminiBlob, GeminiFunctionCall, GeminiFunctionResponse, GeminiPart};

#[derive(Deserialize)]
struct TextFields {
    #[serde(default)]
    text: String,
    #[serde(default, rename = "thoughtSignature", alias = "thought_signature")]
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
struct ThoughtFields {
    #[serde(deserialize_with = "super::reasoning::deserialize_true")]
    thought: bool,
    #[serde(flatten)]
    text: TextFields,
}

#[derive(Deserialize)]
struct CallFields {
    #[serde(rename = "functionCall")]
    function_call: GeminiFunctionCall,
    #[serde(default, rename = "thoughtSignature", alias = "thought_signature")]
    thought_signature: Option<String>,
}

impl<'de> Deserialize<'de> for GeminiPart {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Ok(Self::Unknown(value));
        };
        let payloads = [
            "text",
            "inline_data",
            "inlineData",
            "functionCall",
            "functionResponse",
        ];
        let present = payloads
            .iter()
            .filter(|key| object.contains_key(**key))
            .count();
        if present > 1 {
            return Err(D::Error::custom(
                "Gemini part contains multiple payload kinds",
            ));
        }
        let thought = match object.get("thought") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err(D::Error::custom("Gemini thought flag must be boolean")),
        };
        let signature_only = present == 0
            && (object.contains_key("thoughtSignature")
                || object.contains_key("thought_signature"));
        if object.contains_key("text") || signature_only {
            if thought {
                let fields: ThoughtFields = serde_json::from_value(value)
                    .map_err(|_| D::Error::custom("invalid Gemini thought part"))?;
                return Ok(Self::Thought {
                    text: fields.text.text,
                    thought: fields.thought,
                    thought_signature: fields.text.thought_signature,
                });
            }
            let fields: TextFields = serde_json::from_value(value)
                .map_err(|_| D::Error::custom("invalid Gemini text part"))?;
            return Ok(match fields.thought_signature {
                Some(thought_signature) => Self::SignedText {
                    text: fields.text,
                    thought_signature,
                },
                None => Self::Text { text: fields.text },
            });
        }
        if object.contains_key("functionCall") {
            let fields: CallFields = serde_json::from_value(value)
                .map_err(|_| D::Error::custom("invalid Gemini function call"))?;
            if !fields.function_call.args.is_object() {
                return Err(D::Error::custom(
                    "Gemini function arguments must be an object",
                ));
            }
            return Ok(Self::FunctionCall {
                function_call: fields.function_call,
                thought_signature: fields.thought_signature,
            });
        }
        for key in ["inline_data", "inlineData"] {
            if let Some(blob) = value.get_mut(key) {
                let inline_data: GeminiBlob = serde_json::from_value(blob.take())
                    .map_err(|_| D::Error::custom("invalid Gemini inline data"))?;
                return Ok(Self::InlineData { inline_data });
            }
        }
        if let Some(response) = value.get_mut("functionResponse") {
            let function_response: GeminiFunctionResponse = serde_json::from_value(response.take())
                .map_err(|_| D::Error::custom("invalid Gemini function response"))?;
            return Ok(Self::FunctionResponse { function_response });
        }
        Ok(Self::Unknown(value))
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn response_parse_error(error: serde_json::Error) -> crate::error::Error {
    // Neither thought text nor opaque signatures belong in parse diagnostics.
    crate::error::Error::api(format!(
        "JSON parse error in Gemini response at line {} column {}",
        error.line(),
        error.column()
    ))
}
