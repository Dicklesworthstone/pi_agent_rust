//! Native clarification questions over ACP's negotiated form elicitation.
//! Request ownership stays on one connection and one live prompt; a late
//! editor response can never satisfy a different question or authorize a tool.

use super::*;
use crate::ask::{AskAnswer, AskPolicy, AskRequest, AskResponse, AskTool};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 128;
const MAX_FORM_BYTES: usize = 128 * 1024;
const MAX_OTHER_CHARS: usize = 8192;

struct Pending {
    session: String,
    id: Value,
    sender: oneshot::Sender<Value>,
}

#[derive(Default)]
struct State {
    negotiated: bool,
    form: bool,
    pending: HashMap<String, Pending>,
}

#[derive(Default)]
pub(super) struct Requests {
    state: StdMutex<State>,
}

impl Requests {
    pub(super) fn negotiate(&self, params: &Value) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Initialization describes this connection once. Reinitialization must
        // not silently grant a capability to an already-created session.
        if !state.negotiated {
            state.negotiated = true;
            state.form = params["clientCapabilities"]["elicitation"]["form"].is_object();
        }
    }

    pub(super) fn supported(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.form)
    }

    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.form = false;
        state.pending.clear();
    }

    pub(super) fn cancel_session(&self, session: &str) -> Vec<Value> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .extract_if(|_, pending| pending.session == session)
            .map(|(_, pending)| pending.id)
            .collect()
    }

    pub(super) fn route_response(&self, message: &Value, cx: &AgentCx) -> bool {
        if message["jsonrpc"].as_str() != Some("2.0") || message.get("method").is_some() {
            return false;
        }
        let Some(id) = message.get("id") else {
            return false;
        };
        let key = json_rpc_id_key(id);
        let pending = self.state.lock().ok().and_then(|mut state| state.pending.remove(&key));
        let Some(pending) = pending else {
            return false;
        };
        // Never send arbitrary editor error messages or metadata to the model.
        // The waiter validates the exact submitted values against its own form.
        let response = match (message.get("result"), message.get("error")) {
            (Some(result), None) if result.is_object() => result.clone(),
            _ => json!({"invalidResponse": true}),
        };
        let _ = pending.sender.send(cx.cx(), response);
        true
    }
}

#[derive(Clone)]
struct Client {
    output: AcpOutput,
    session: String,
    timeout: Duration,
}

struct RequestGuard {
    output: AcpOutput,
    key: String,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let pending = self.output.shared.elicitation.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending.remove(&self.key);
        if let Some(pending) = pending {
            // Drop the waiter before publishing cancellation. Unknown and late
            // replies are harmless even if the editor cannot dismiss its form.
            let id = pending.id;
            drop(pending.sender);
            let _ = self.output.send(json_rpc_notification(
                "$/cancel_request",
                json!({"requestId": id}),
            ));
        }
    }
}

impl Client {
    async fn ask(&self, tool_call_id: &str, request: AskRequest) -> Result<AskResponse> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        let request = crate::ask::validate_request(request)?;
        let id = json!(format!("pi-elicitation-{}", uuid::Uuid::new_v4()));
        let key = json_rpc_id_key(&id);
        let frame = json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": {
                "sessionId": self.session,
                "toolCallId": tool_call_id,
                "mode": "form",
                "message": "Clarify the implementation before the agent continues. Review each answer; choose Other to provide your own. Do not enter passwords, API keys, access tokens, payment credentials or other secrets.",
                "requestedSchema": form_schema(&request),
            },
        }).to_string();
        if frame.len() > MAX_FORM_BYTES {
            return Err(Error::tool("ask", "Clarification form exceeds the editor size limit"));
        }
        let (sender, mut receiver) = oneshot::channel();
        {
            // Match permission/filesystem lock order. A cancel or steering
            // notification cannot overtake publication of the original form.
            let mut transport = self.output.shared.state.lock()
                .map_err(|_| Error::session("ACP output state unavailable"))?;
            if transport.stopping || transport.failure.is_some()
                || !transport.prompts.get(&self.session)
                    .is_some_and(|prompt| prompt.input.permits_tool())
            {
                return Err(Error::tool("ask", "Clarification cancelled before publication"));
            }
            let mut state = self.output.shared.elicitation.state.lock()
                .map_err(|_| Error::session("ACP elicitation registry unavailable"))?;
            if !state.form {
                return Err(Error::tool("ask", "Editor did not advertise form elicitation"));
            }
            if state.pending.len() >= MAX_PENDING
                || state.pending.values().any(|pending| pending.session == self.session)
            {
                return Err(Error::tool("ask", "An editor clarification is already pending"));
            }
            state.pending.insert(key.clone(), Pending {
                session: self.session.clone(), id, sender,
            });
            drop(state);
            if self.output.sender.try_send(frame).is_err() {
                self.output.shared.record_failure(
                    &mut transport, "ACP clarification could not be delivered",
                );
                return Err(Error::tool("ask", "Editor clarification could not be delivered"));
            }
        }
        let _guard = RequestGuard { output: self.output.clone(), key };
        let response = timeout(
            wall_now(), self.timeout, Box::pin(receiver.recv(owner.cx())),
        ).await
            .map_err(|_| Error::tool("ask", "Editor clarification timed out"))?
            .map_err(|_| Error::tool("ask", "Editor clarification was cancelled or disconnected"))?;
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        parse_response(&request, &response)
    }
}

fn form_schema(request: &AskRequest) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (index, question) in request.questions.iter().enumerate() {
        let choice_key = format!("question_{index}");
        let other_key = format!("other_{index}");
        let mut choices: Vec<Value> = question.options.iter().enumerate().map(|(option, value)| {
            let title = if question.recommended == Some(option) {
                format!("{} (Recommended)", value.label)
            } else {
                value.label.clone()
            };
            json!({"const": format!("option_{option}"), "title": title})
        }).collect();
        choices.push(json!({"const": "other", "title": "Other (write your answer below)"}));
        let mut descriptions = Vec::new();
        if let Some(header) = &question.header {
            descriptions.push(header.clone());
        }
        for option in &question.options {
            if let Some(description) = &option.description {
                descriptions.push(format!("{}: {description}", option.label));
            }
        }
        descriptions.push("Choose Other by itself and fill in the accompanying text field, or select the offered option(s). Leave the Other field blank for an offered choice.".into());
        let mut choice = if question.multi {
            json!({
                "type": "array", "minItems": 1, "maxItems": question.options.len(),
                "items": {"anyOf": choices},
            })
        } else {
            json!({"type": "string", "oneOf": choices})
        };
        choice["title"] = json!(question.question);
        choice["description"] = json!(descriptions.join("\n"));
        if let Some(recommended) = question.recommended {
            let value = format!("option_{recommended}");
            choice["default"] = if question.multi { json!([value]) } else { json!(value) };
        }
        properties.insert(choice_key.clone(), choice);
        properties.insert(other_key, json!({
            "type": "string",
            "title": format!("Other answer for question {}", index + 1),
            "description": "Only fill this in when you select Other. Do not include secrets or credentials.",
            "maxLength": MAX_OTHER_CHARS,
        }));
        required.push(choice_key);
    }
    json!({"type": "object", "properties": properties, "required": required})
}

fn parse_response(request: &AskRequest, response: &Value) -> Result<AskResponse> {
    let invalid = || Error::tool("ask", "Editor returned an invalid clarification response");
    match response["action"].as_str() {
        Some("decline") => return Err(Error::tool("ask", "User declined the clarification")),
        Some("cancel") => return Ok(AskResponse { answers: Vec::new(), dismissed: true }),
        Some("accept") if response.get("invalidResponse").is_none() => {}
        _ => return Err(invalid()),
    }
    let content = response["content"].as_object().ok_or_else(invalid)?;
    let allowed: HashSet<String> = (0..request.questions.len())
        .flat_map(|index| [format!("question_{index}"), format!("other_{index}")])
        .collect();
    if content.keys().any(|key| !allowed.contains(key)) {
        return Err(invalid());
    }
    let mut answers = Vec::with_capacity(request.questions.len());
    for (index, question) in request.questions.iter().enumerate() {
        let choice_key = format!("question_{index}");
        let other_key = format!("other_{index}");
        let values: Vec<&str> = if question.multi {
            content.get(&choice_key).and_then(Value::as_array).ok_or_else(invalid)?
                .iter().map(Value::as_str).collect::<Option<_>>().ok_or_else(invalid)?
        } else {
            vec![content.get(&choice_key).and_then(Value::as_str).ok_or_else(invalid)?]
        };
        let mut seen = HashSet::new();
        if values.is_empty() || values.len() > question.options.len()
            || values.iter().any(|value| !seen.insert(*value))
        {
            return Err(invalid());
        }
        let other = match content.get(&other_key) {
            None => None,
            Some(Value::String(text)) if text.chars().count() <= MAX_OTHER_CHARS => {
                (!text.trim().is_empty()).then(|| text.clone())
            }
            _ => return Err(invalid()),
        };
        let mut selected = Vec::new();
        if values.contains(&"other") {
            if values.len() != 1 || other.is_none() {
                return Err(invalid());
            }
        } else {
            if other.is_some() {
                return Err(invalid());
            }
            for value in values {
                let option = question.options.iter().enumerate()
                    .find(|(index, _)| value == format!("option_{index}"))
                    .map(|(_, option)| option.label.clone())
                    .ok_or_else(invalid)?;
                selected.push(option);
            }
        }
        answers.push(AskAnswer {
            question_id: crate::ask::effective_question_id(question, index),
            selected,
            other,
        });
    }
    Ok(AskResponse { answers, dismissed: false })
}

/// The native tool retains validation, formatting and ordered effects. The
/// ACP adapter binds each invocation to its real tool-call identity and uses
/// an installed handler even when a headless recommended policy is configured.
struct EditorAsk {
    native: AskTool,
    client: Client,
}

pub(super) fn tool(output: AcpOutput, session: &str) -> Box<dyn Tool> {
    Box::new(EditorAsk {
        native: AskTool::new(AskPolicy::Error),
        client: Client { output, session: session.to_owned(), timeout: REQUEST_TIMEOUT },
    })
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for EditorAsk {
    fn name(&self) -> &str { "ask" }
    fn label(&self) -> &str { self.native.label() }
    fn description(&self) -> &str {
        "Ask the user non-sensitive implementation questions through the editor. Use only when different answers materially change the work. Offer 2-5 options, optionally multiple selections, and mark a recommended index. The user can supply an Other answer or decline. Never request passwords, API keys, access tokens, private keys, recovery codes or payment credentials; this form is not an authentication flow."
    }
    fn parameters(&self) -> Value { self.native.parameters() }
    fn effects(&self) -> ToolEffects { self.native.effects() }

    async fn execute(
        &self, tool_call_id: &str, input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let native = AskTool::new(AskPolicy::Error);
        let client = self.client.clone();
        let call_id = tool_call_id.to_owned();
        native.set_handler(Arc::new(move |request| {
            let client = client.clone();
            let call_id = call_id.clone();
            Box::pin(async move { client.ask(&call_id, request).await })
        }));
        native.execute(tool_call_id, input, on_update).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;

    fn question_batch() -> AskRequest {
        crate::ask::validate_request(serde_json::from_value(json!({
            "questions": [
                {
                    "id": "scope", "question": "How much should change?", "header": "Scope",
                    "options": [
                        {"label": "Small change", "description": "Preserve the implementation"},
                        {"label": "Full redesign", "description": "Replace the internals"},
                    ],
                    "recommended": 0,
                },
                {
                    "id": "checks", "question": "Which checks matter?", "multi": true,
                    "options": [{"label": "Unit tests"}, {"label": "Integration tests"}],
                    "recommended": 1,
                },
                {
                    "id": "constraint", "question": "Which constraint should guide the plan?",
                    "options": [{"label": "Latency"}, {"label": "Memory"}],
                },
            ],
        })).unwrap()).unwrap()
    }

    fn accepted_content() -> Value {
        json!({
            "question_0": "option_1",
            "question_1": ["option_0", "option_1"],
            "question_2": "other",
            "other_2": "Preserve the public API",
        })
    }

    fn connection(capability: Value) -> (AcpOutput, std::sync::mpsc::Receiver<String>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(32);
        let output = AcpOutput::new(sender);
        output.shared.elicitation.negotiate(&json!({
            "clientCapabilities": {"elicitation": capability},
        }));
        (output, receiver)
    }

    fn begin(output: &AcpOutput, session: &str) -> AcpPromptGuard {
        let (abort, _) = AbortHandle::new();
        output.begin_prompt(session.into(), abort).unwrap()
    }

    fn frame(receiver: &std::sync::mpsc::Receiver<String>) -> Value {
        serde_json::from_str(&receiver.try_recv().expect("published editor request")).unwrap()
    }

    fn reply(output: &AcpOutput, id: &Value, result: Value) -> bool {
        output.shared.elicitation.route_response(
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            &AgentCx::for_current_or_request(),
        )
    }

    #[test]
    fn only_explicit_form_objects_enable_clarification() {
        for capability in [
            Value::Null, json!({}), json!({"url": {}}),
            json!({"form": null}), json!({"form": false}), json!({"form": true}),
            json!({"form": "supported"}), json!({"form": []}),
        ] {
            let (output, _) = connection(capability);
            assert!(!output.shared.elicitation.supported());
            output.shared.elicitation.negotiate(&json!({
                "clientCapabilities": {"elicitation": {"form": {}}},
            }));
            assert!(!output.shared.elicitation.supported(), "initialization is connection-scoped");
        }
        let (output, _) = connection(json!({"form": {}}));
        assert!(output.shared.elicitation.supported());
        output.shared.elicitation.close();
        assert!(!output.shared.elicitation.supported());
    }

    #[test]
    fn form_fields_preserve_choices_descriptions_recommendations_and_other() {
        let schema = form_schema(&question_batch());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["question_0", "question_1", "question_2"]));
        let fields = schema["properties"].as_object().unwrap();
        assert_eq!(fields.len(), 6);
        assert!(fields.values().all(|field| matches!(field["type"].as_str(), Some("string" | "array"))));
        assert_eq!(fields["question_0"]["oneOf"][0]["title"], "Small change (Recommended)");
        assert_eq!(fields["question_0"]["oneOf"][1]["title"], "Full redesign");
        assert_eq!(fields["question_0"]["oneOf"][2]["const"], "other");
        assert_eq!(fields["question_0"]["default"], "option_0");
        assert!(fields["question_0"]["description"].as_str().unwrap().contains("Replace the internals"));
        assert_eq!(fields["question_1"]["items"]["anyOf"][1]["title"], "Integration tests (Recommended)");
        assert_eq!(fields["question_1"]["default"], json!(["option_1"]));
        assert_eq!(fields["other_2"]["maxLength"], MAX_OTHER_CHARS);
        assert!(fields["question_2"].get("default").is_none());
    }

    #[test]
    fn accepted_forms_require_complete_typed_offered_answers_without_defaulting() {
        let request = question_batch();
        let response = parse_response(&request, &json!({
            "action": "accept", "content": accepted_content(),
        })).unwrap();
        assert_eq!(response.answers[0].question_id, "scope");
        assert_eq!(response.answers[0].selected, vec!["Full redesign"]);
        assert_eq!(response.answers[1].selected, vec!["Unit tests", "Integration tests"]);
        assert!(response.answers[2].selected.is_empty());
        assert_eq!(response.answers[2].other.as_deref(), Some("Preserve the public API"));

        for response in [
            json!({}), json!({"action": "unknown"}), json!({"action": "accept"}),
            json!({"action": "accept", "content": null}),
            json!({"action": "accept", "content": []}),
        ] {
            assert!(parse_response(&request, &response).is_err(), "{response}");
        }
        for (key, invalid) in [
            ("question_0", json!(["option_1"])),
            ("question_0", json!("option_99")),
            ("question_0", json!("option_01")),
            ("question_0", json!("Full redesign")),
            ("question_0", json!("other")),
            ("question_1", json!("option_1")),
            ("question_1", json!([])),
            ("question_1", json!(["option_0", "option_0"])),
            ("question_1", json!(["option_0", "other"])),
            ("question_1", json!([7])),
            ("question_2", Value::Null),
            ("other_0", json!("text without choosing Other")),
            ("other_2", json!(" ")),
            ("other_2", Value::Null),
            ("other_2", json!("x".repeat(MAX_OTHER_CHARS + 1))),
            ("unrequested_question", json!("option_0")),
        ] {
            let mut content = accepted_content();
            content[key] = invalid;
            assert!(parse_response(&request, &json!({
                "action": "accept", "content": content,
            })).is_err(), "must reject malformed field {key}: {content}");
        }
        for key in ["question_0", "question_1", "question_2", "other_2"] {
            let mut content = accepted_content();
            content.as_object_mut().unwrap().remove(key);
            assert!(parse_response(&request, &json!({
                "action": "accept", "content": content,
            })).is_err(), "missing {key} cannot select a recommendation");
        }
        let declined = parse_response(&request, &json!({
            "action": "decline", "content": {"ignored": ["arbitrary", "data"]},
        })).unwrap_err().to_string();
        assert!(declined.contains("declined") && !declined.contains("arbitrary"));
        let cancelled = parse_response(&request, &json!({
            "action": "cancel", "content": accepted_content(),
        })).unwrap();
        assert!(cancelled.dismissed && cancelled.answers.is_empty());
    }

    #[test]
    fn native_adapter_round_trips_real_tool_identity_and_answer_details() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            let (output, received) = connection(json!({"form": {}}));
            let _prompt = begin(&output, "owned-session");
            let ask = tool(output.clone(), "owned-session");
            assert!(ask.effects().requires_ordered_execution());
            assert!(!ask.effects().writes());
            let input = serde_json::to_value(question_batch()).unwrap();
            let mut execution = Box::pin(ask.execute("real-native-call", input, None));
            assert!(futures::poll!(execution.as_mut()).is_pending());
            let shown = frame(&received);
            assert_eq!(shown["method"], "elicitation/create");
            assert_eq!(shown["params"]["sessionId"], "owned-session");
            assert_eq!(shown["params"]["toolCallId"], "real-native-call");
            assert_eq!(shown["params"]["mode"], "form");
            assert!(shown.get("options").is_none(), "clarification is not a permission grant");
            assert!(reply(&output, &shown["id"], json!({
                "action": "accept", "content": accepted_content(),
            })));
            let result = execution.await.unwrap();
            assert!(!result.is_error);
            let details = result.details.unwrap();
            assert_eq!(details["schema"], crate::ask::ASK_RESPONSE_SCHEMA);
            assert_eq!(details["autoAnswered"], false);
            assert_eq!(details["answers"][0]["selected"], json!(["Full redesign"]));
            assert_eq!(details["answers"][2]["other"], "Preserve the public API");
            assert!(received.try_recv().is_err(), "settled forms are not cancelled again");
            assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
            assert!(!reply(&output, &shown["id"], json!({
                "action": "accept", "content": accepted_content(),
            })), "duplicate answers cannot be reused");
        });
    }

    #[test]
    fn session_cancel_and_steering_retire_only_the_owned_form() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            for steer in [false, true] {
                let (output, received) = connection(json!({"form": {}}));
                let _first_prompt = begin(&output, "first");
                let _second_prompt = begin(&output, "second");
                let first_client = Client { output: output.clone(), session: "first".into(), timeout: REQUEST_TIMEOUT };
                let second_client = Client { output: output.clone(), session: "second".into(), timeout: REQUEST_TIMEOUT };
                let mut first = Box::pin(first_client.ask("first-call", question_batch()));
                let mut second = Box::pin(second_client.ask("second-call", question_batch()));
                assert!(futures::poll!(first.as_mut()).is_pending());
                let first_frame = frame(&received);
                assert!(futures::poll!(second.as_mut()).is_pending());
                let second_frame = frame(&received);
                assert_ne!(first_frame["id"], second_frame["id"]);
                assert!(first_client.ask("overlapping", question_batch()).await.is_err());
                assert!(received.try_recv().is_err(), "one modal slot per session");
                let (foreign, _) = connection(json!({"form": {}}));
                assert!(!reply(&foreign, &first_frame["id"], json!({
                    "action": "accept", "content": accepted_content(),
                })), "another connection cannot answer this request");
                output.enqueue_steering("first", steering::Input::parse(
                    &json!({"prompt": "after this", "_meta": {"steering": {"queue": "followUp"}}}),
                    Some(json!(41)), &Config::default(),
                ).unwrap());
                assert!(received.try_recv().is_err(), "follow-ups do not dismiss the current question");
                assert_eq!(output.shared.elicitation.state.lock().unwrap().pending.len(), 2);
                if steer {
                    output.enqueue_steering("first", steering::Input::parse(
                        &json!({"prompt": "inspect a different approach"}),
                        Some(json!(42)), &Config::default(),
                    ).unwrap());
                } else {
                    output.abort_prompt("first");
                }
                let cancellation = frame(&received);
                assert_eq!(cancellation["method"], "$/cancel_request");
                assert_eq!(cancellation["params"]["requestId"], first_frame["id"]);
                assert!(first.await.is_err());
                assert!(!reply(&output, &first_frame["id"], json!({
                    "action": "accept", "content": accepted_content(),
                })), "a late accepted answer stays retired");
                assert_eq!(output.shared.elicitation.state.lock().unwrap().pending.len(), 1);
                assert!(reply(&output, &second_frame["id"], json!({
                    "action": "accept", "content": accepted_content(),
                })));
                assert_eq!(second.await.unwrap().answers[0].selected, vec!["Full redesign"]);
                assert!(first_client.ask("after-cancel", question_batch()).await.is_err());
                assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
            }
        });
    }

    #[test]
    fn abandoned_timed_out_and_closed_forms_never_fall_back_to_recommendations() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            for finish in ["drop", "timeout", "transport", "shutdown"] {
                let (output, received) = connection(json!({"form": {}}));
                let prompt = begin(&output, "session");
                let client = Client {
                    output: output.clone(), session: "session".into(),
                    timeout: if finish == "timeout" { Duration::ZERO } else { REQUEST_TIMEOUT },
                };
                let mut request = Box::pin(client.ask("call", question_batch()));
                let shown = if finish == "timeout" {
                    assert!(request.await.unwrap_err().to_string().contains("timed out"));
                    frame(&received)
                } else {
                    assert!(futures::poll!(request.as_mut()).is_pending());
                    let shown = frame(&received);
                    match finish {
                        "drop" => drop(request),
                        "transport" => {
                            output.shared.fail("test editor disconnected");
                            assert!(request.await.is_err());
                        }
                        "shutdown" => {
                            drop(prompt);
                            output.finish_prompts().await.unwrap();
                            assert!(request.await.is_err());
                        }
                        _ => unreachable!(),
                    }
                    shown
                };
                assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
                assert!(!reply(&output, &shown["id"], json!({
                    "action": "accept", "content": accepted_content(),
                })));
                if matches!(finish, "drop" | "timeout") {
                    let cancellation = frame(&received);
                    assert_eq!(cancellation["method"], "$/cancel_request");
                    assert_eq!(cancellation["params"]["requestId"], shown["id"]);
                }
            }
        });
    }

    #[test]
    fn publication_failures_clear_inserted_waiters_and_stop_new_forms() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            for full in [false, true] {
                let (output, received) = connection(json!({"form": {}}));
                let _prompt = begin(&output, "session");
                if full {
                    for _ in 0..32 {
                        output.send("{}".into()).unwrap();
                    }
                } else {
                    drop(received);
                }
                let client = Client {
                    output: output.clone(), session: "session".into(), timeout: REQUEST_TIMEOUT,
                };
                let error = client.ask("undeliverable", question_batch()).await.unwrap_err().to_string();
                assert!(error.contains("could not be delivered"));
                assert!(output.failure_reason().is_some());
                assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
                assert!(client.ask("after-failure", question_batch()).await.is_err());
            }
        });
    }

    #[test]
    fn unsupported_or_oversized_forms_never_publish_a_request() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            for supported in [false, true] {
                let (output, received) = connection(if supported { json!({"form": {}}) } else { json!({}) });
                let _prompt = begin(&output, "session");
                let client = Client {
                    output: output.clone(), session: "session".into(), timeout: REQUEST_TIMEOUT,
                };
                let mut request = question_batch();
                if supported {
                    request.questions[0].question = "x".repeat(MAX_FORM_BYTES);
                }
                assert!(client.ask("not-admitted", request).await.is_err());
                assert!(received.try_recv().is_err());
                assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
            }
        });
    }

    #[test]
    fn malformed_editor_envelopes_fail_the_exact_waiter_without_echoing_errors() {
        RuntimeBuilder::current_thread().build().unwrap().block_on(async {
            for envelope in [
                json!({"error": {"code": -32603, "message": "PRIVATE-EDITOR-DETAIL"}}),
                json!({"result": {"action": "accept", "content": accepted_content()}, "error": {}}),
                json!({"result": null}),
                json!({"result": {"action": "accept", "content": {
                    "question_0": "PRIVATE-EDITOR-DETAIL",
                }}}),
                json!({}),
            ] {
                let (output, received) = connection(json!({"form": {}}));
                let _prompt = begin(&output, "session");
                let ask = tool(output.clone(), "session");
                let mut execution = Box::pin(ask.execute(
                    "call", serde_json::to_value(question_batch()).unwrap(), None,
                ));
                assert!(futures::poll!(execution.as_mut()).is_pending());
                let shown = frame(&received);
                let mut response = envelope;
                response["jsonrpc"] = json!("2.0");
                response["id"] = shown["id"].clone();
                assert!(output.shared.elicitation.route_response(
                    &response, &AgentCx::for_current_or_request(),
                ));
                let error = execution.await.unwrap_err().to_string();
                assert!(error.contains("invalid clarification"));
                assert!(!error.contains("PRIVATE-EDITOR-DETAIL"));
                assert!(output.shared.elicitation.state.lock().unwrap().pending.is_empty());
            }
        });
    }
}
