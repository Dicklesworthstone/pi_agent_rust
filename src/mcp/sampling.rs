//! A server may sample only through the live tool caller and two host reviews.
//!
//! The transport owns routing, never a provider or credentials. This borrowed
//! capability keeps model selection, privacy, cancellation and spend on the
//! initiating Agent without exposing its transcript or tool registry.

use serde_json::{Value, json};

use super::elicitation::{bounded, display, rpc_error};
use crate::ask::{AskOption, AskQuestion, AskRequest, AskTool};
use crate::error::{Error, Result};
use crate::model::StopReason;
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::{
    ToolInvocationHost, ToolSamplingMessage, ToolSamplingModel, ToolSamplingRequest,
    ToolSamplingRole,
};

const MAX_REQUEST_BYTES: usize = 32 * 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_MESSAGES: usize = 64;
const DEFAULT_MAX_TOKENS: u32 = 4096;
const GENERATE: &str = "Generate this reply";
const DECLINE: &str = "Decline request";
const SEND: &str = "Send reply to server";
const DISCARD: &str = "Discard reply";
const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";

/// This value must not outlive the native tool invocation. In particular,
/// neither a transport reader nor a background job receives its host borrow.
pub(crate) struct SamplingContext<'a> {
    host: &'a dyn ToolInvocationHost,
    model: ToolSamplingModel,
    privacy: AuxiliaryPrivacySnapshot,
}

impl<'a> SamplingContext<'a> {
    pub(crate) fn capture(
        host: &'a dyn ToolInvocationHost,
        privacy: AuxiliaryPrivacySnapshot,
    ) -> Result<Self> {
        let model = host.sampling_model()?;
        if model.provider.is_empty() || model.model.is_empty() || model.session_id.is_empty() {
            return Err(Error::tool("mcp", "sampling requires an active model and session"));
        }
        Ok(Self { host, model, privacy })
    }

    fn is_current(&self) -> bool {
        self.host.sampling_model().is_ok_and(|model| {
            model.provider == self.model.provider
                && model.model == self.model.model
                && model.session_id == self.model.session_id
                && model.max_tokens == self.model.max_tokens
                && model.temperature == self.model.temperature
        })
    }
}

fn invalid(id: &Value) -> Value {
    rpc_error(id, -32602, "sampling requires a bounded text-only prompt, no tools or context, and no nonempty stop sequences")
}

pub(super) fn rejected(id: &Value) -> Value {
    rpc_error(id, -1, "sampling was declined, cancelled, or is unavailable for this caller")
}

fn text_content(value: &Value) -> Option<String> {
    fn block(value: &Value) -> Option<&str> {
        let object = value.as_object()?;
        if object.get("type")?.as_str()? != "text" {
            return None;
        }
        object.get("text")?.as_str()
    }
    match value {
        Value::Array(values) if !values.is_empty() && values.len() <= MAX_MESSAGES => {
            values.iter().map(block).collect::<Option<Vec<_>>>().map(|parts| parts.join("\n"))
        }
        Value::Object(_) => block(value).map(str::to_string),
        _ => None,
    }
}

fn model_preferences(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    for key in ["costPriority", "speedPriority", "intelligencePriority"] {
        if object.get(key).is_some_and(|value| {
            value.as_f64().is_none_or(|number| !(0.0..=1.0).contains(&number))
        }) {
            return false;
        }
    }
    object.get("hints").is_none_or(|value| {
        value.as_array().is_some_and(|hints| {
            hints.len() <= MAX_MESSAGES && hints.iter().all(|hint| {
                hint.as_object().is_some_and(|hint| {
                    hint.get("name").is_none_or(Value::is_string)
                })
            })
        })
    })
}

fn parse(params: &Value, model: &ToolSamplingModel) -> Option<ToolSamplingRequest> {
    if !bounded(params, MAX_REQUEST_BYTES) {
        return None;
    }
    let object = params.as_object()?;
    if object.contains_key("tools") || object.contains_key("toolChoice")
        || object.contains_key("task")
        || object.get("includeContext").is_some_and(|value| value.as_str() != Some("none"))
        || object.get("stopSequences").is_some_and(|value| {
            value.as_array().is_none_or(|values| !values.is_empty())
        })
        || object.get("metadata").is_some_and(|value| !value.is_object())
        || object.get("_meta").is_some_and(|value| !value.is_object())
        || object.get("modelPreferences").is_some_and(|value| !model_preferences(value))
    {
        return None;
    }
    let requested_tokens = u32::try_from(object.get("maxTokens")?.as_u64()?).ok()?;
    let max_tokens = requested_tokens.min(model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS));
    if max_tokens == 0 {
        return None;
    }
    let temperature = match object.get("temperature") {
        Some(value) => {
            let number = value.as_f64()?;
            if !(0.0..=2.0).contains(&number) {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)]
            Some(number as f32)
        }
        None => model.temperature,
    };
    let system_prompt = match object.get("systemPrompt") {
        Some(value) => Some(value.as_str()?.to_string()),
        None => None,
    };
    let messages = object.get("messages")?.as_array()?;
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return None;
    }
    let messages = messages.iter().map(|message| {
        let role = match message.get("role")?.as_str()? {
            "user" => ToolSamplingRole::User,
            "assistant" => ToolSamplingRole::Assistant,
            _ => return None,
        };
        Some(ToolSamplingMessage { role, text: text_content(message.get("content")?)? })
    }).collect::<Option<Vec<_>>>()?;
    Some(ToolSamplingRequest { system_prompt, messages, max_tokens, temperature })
}

fn protect(request: &mut ToolSamplingRequest, privacy: &AuxiliaryPrivacySnapshot) -> Result<()> {
    let parts: Vec<&str> = request.system_prompt.iter().map(String::as_str)
        .chain(request.messages.iter().map(|message| message.text.as_str())).collect();
    let projected = privacy.project_inputs(&parts)?;
    let mut projected = projected.into_iter();
    if let Some(system_prompt) = request.system_prompt.as_mut() {
        *system_prompt = projected.next().ok_or_else(|| Error::tool("mcp", "sampling privacy projection failed"))?;
    }
    for message in &mut request.messages {
        message.text = projected.next().ok_or_else(|| Error::tool("mcp", "sampling privacy projection failed"))?;
    }
    Ok(())
}

fn prompt_value(request: &ToolSamplingRequest) -> Value {
    let messages: Vec<Value> = request.messages.iter().map(|message| {
        json!({"role":match &message.role {
            ToolSamplingRole::User => "user",
            ToolSamplingRole::Assistant => "assistant",
        }, "content":{"type":"text", "text":message.text}})
    }).collect();
    let mut value = json!({"messages":messages,"maxTokens":request.max_tokens});
    if let Some(system_prompt) = &request.system_prompt {
        value["systemPrompt"] = json!(system_prompt);
    }
    if let Some(temperature) = request.temperature {
        value["temperature"] = json!(temperature);
    }
    value
}

enum Answer {
    Choice(String),
    Edit(String),
    Cancel,
}

async fn prompt(ask: &AskTool, id: &str, question: String, choices: [&str; 2]) -> Answer {
    let request = AskRequest { questions: vec![AskQuestion {
        id: Some(id.to_string()),
        question,
        header: Some("MCP sampling".to_string()),
        options: choices.into_iter().map(|label| AskOption {
            label: label.to_string(), description: None,
        }).collect(),
        recommended: None,
        multi: false,
    }] };
    let Ok(response) = ask.prompt_installed(request).await else {
        return Answer::Cancel;
    };
    if response.dismissed || response.answers.len() != 1 || response.answers[0].question_id != id {
        return Answer::Cancel;
    }
    match (response.answers[0].selected.as_slice(), response.answers[0].other.as_ref()) {
        ([choice], None) => Answer::Choice(choice.clone()),
        ([], Some(text)) if text.len() <= MAX_REPLY_BYTES => Answer::Edit(text.clone()),
        _ => Answer::Cancel,
    }
}

/// Called only by the invocation-owned observer. Dropping this future drops
/// the pending Ask lease or provider stream; it never schedules detached work.
#[allow(clippy::too_many_lines)]
pub(super) async fn respond(
    id: &Value,
    params: &Value,
    related_task: Option<&str>,
    ask: Option<&AskTool>,
    sampling: Option<&SamplingContext<'_>>,
    server: &str,
    current: &(dyn Fn() -> bool + Send + Sync),
) -> Value {
    let Some(sampling) = sampling else {
        return rejected(id);
    };
    let Some(mut request) = parse(params, &sampling.model) else {
        return invalid(id);
    };
    let Some(ask) = ask else {
        return rejected(id);
    };
    let mut privacy = sampling.privacy.clone();
    if privacy.capture_json(params).is_err() || protect(&mut request, &privacy).is_err() {
        return rejected(id);
    }
    let origin = format!("MCP server \"{}\" requests a model reply using {} / {}.\nGeneration uses this session's cost budget. Model preferences are advisory; server metadata and your conversation are not sent.\nControls, markup, and URL punctuation are escaped for display.",
        display(server), display(&sampling.model.provider), display(&sampling.model.model));
    let mut approved = false;
    for _ in 0..4 {
        if !current() || !sampling.is_current() {
            return rejected(id);
        }
        let value = prompt_value(&request);
        if !bounded(&value, MAX_REQUEST_BYTES) {
            return invalid(id);
        }
        let question = format!("{origin}\n\nReview the complete provider prompt and effective settings below. Generate spends tokens and produces a private draft; another review controls whether the server receives it. Use Other to replace this JSON prompt, then review it again.\n\n{}", display(&value.to_string()));
        match prompt(ask, "mcp_sample_prompt", question, [GENERATE, DECLINE]).await {
            Answer::Choice(choice) if choice == GENERATE => {
                approved = true;
                break;
            }
            Answer::Edit(text) if text.len() <= MAX_REQUEST_BYTES => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    return invalid(id);
                };
                let Some(mut edited) = parse(&value, &sampling.model) else {
                    return invalid(id);
                };
                if privacy.capture_json(&value).is_err() || protect(&mut edited, &privacy).is_err() {
                    return rejected(id);
                }
                request = edited;
            }
            _ => return rejected(id),
        }
    }
    if !approved || !current() || !sampling.is_current() {
        return rejected(id);
    }
    if sampling.host.check_sampling_session(&sampling.model.session_id).await.is_err()
        || !current() || !sampling.is_current()
    {
        return rejected(id);
    }
    let Ok(result) = sampling.host.sample(request, &sampling.model.session_id).await else {
        // Provider errors may contain authentication, endpoints, or request
        // details. The server receives only a bounded, non-secret refusal.
        return rpc_error(id, -1, "sampling could not produce an approved text reply");
    };
    let stop_reason = match result.stop_reason {
        StopReason::Stop => "endTurn",
        StopReason::Length => "maxTokens",
        _ => return rpc_error(id, -1, "sampling did not complete with a text reply"),
    };
    if result.text.len() > MAX_REPLY_BYTES {
        return rpc_error(id, -1, "sampling reply exceeded the client limit");
    }
    let Ok(mut projected) = privacy.project_inputs(&[result.text.as_str()]) else {
        return rejected(id);
    };
    let Some(mut text) = projected.pop() else {
        return rejected(id);
    };
    for _ in 0..4 {
        if text.len() > MAX_REPLY_BYTES || !current() || !sampling.is_current() {
            return rejected(id);
        }
        let question = format!("{origin}\n\nReview the complete reply below (stop reason: {stop_reason}). Send releases exactly this text to the server. Discard withholds it; generation costs have already been charged. Use Other to replace the reply text, then review it again.\n\n{}", display(&text));
        match prompt(ask, "mcp_sample_review", question, [SEND, DISCARD]).await {
            Answer::Choice(choice) if choice == SEND => {
                if !current() || !sampling.is_current()
                    || sampling.host.check_sampling_session(&sampling.model.session_id).await.is_err()
                    || !current() || !sampling.is_current()
                {
                    return rejected(id);
                }
                let mut result = json!({"role":"assistant", "content":{"type":"text", "text":text},
                    "model":sampling.model.model, "stopReason":stop_reason});
                if let Some(task) = related_task {
                    result["_meta"] = json!({(RELATED_TASK):{"taskId":task}});
                }
                return json!({"jsonrpc":"2.0", "id":id, "result":result});
            }
            Answer::Edit(edited) => {
                let Ok(mut projected) = privacy.project_inputs(&[edited.as_str()]) else {
                    return rejected(id);
                };
                let Some(edited) = projected.pop() else {
                    return rejected(id);
                };
                text = edited;
            }
            _ => return rejected(id),
        }
    }
    rejected(id)
}
