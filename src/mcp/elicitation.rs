//! Form elicitation belongs to the tool invocation that admitted it.
//!
//! Transport readers only enqueue bounded requests. User interaction and
//! responses run on the initiating caller, with its original deadline,
//! capabilities, trust decision, and protocol-session generation. A server
//! cannot obtain a new request context by sending an unsolicited prompt.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::{Future, poll_fn};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::manager::McpOutputSchema;
use super::transport::{McpTransport, RequestDispatch};
use crate::ask::{AskOption, AskQuestion, AskRequest, AskTool};
use crate::error::{Error, Result};

const MAX_CALLS: usize = 64;
const MAX_REQUESTS_PER_CALL: usize = 8;
const MAX_FORM_BYTES: usize = 32 * 1024;
const MAX_CONTENT_BYTES: usize = 16 * 1024;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_FIELDS: usize = 16;
const MAX_CHOICES: usize = 128;
const RELATED_TASK: &str = "io.modelcontextprotocol/related-task";
const DECLINE: &str = "Decline request";
const CANCEL: &str = "Cancel request";
const OMIT: &str = "Leave this optional field out";
const KEEP: &str = "Use the displayed value";
const SUBMIT: &str = "Submit these values";
const EDIT: &str = "Edit answers";

#[cfg(test)]
mod tests;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failure(message: &str) -> Error {
    Error::tool("mcp", format!("[MCP_CANCELLED] {message}"))
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn action(id: &Value, name: &str, related_task: Option<&str>) -> Value {
    let mut result = json!({"action":name});
    if let Some(task) = related_task {
        result["_meta"] = json!({(RELATED_TASK):{"taskId":task}});
    }
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "elicitation limit exceeded")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn within_budget(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    if depth == 0 || *nodes == 0 {
        return false;
    }
    *nodes -= 1;
    match value {
        Value::Array(values) => values
            .iter()
            .all(|value| within_budget(value, depth - 1, nodes)),
        Value::Object(values) => values
            .values()
            .all(|value| within_budget(value, depth - 1, nodes)),
        _ => true,
    }
}

fn bounded(value: &Value, bytes: usize) -> bool {
    let mut nodes = 4096;
    within_budget(value, 8, &mut nodes)
        && serde_json::to_writer(&mut ByteBudget(bytes), value).is_ok()
}

enum TaskOwner {
    Synchronous,
    AwaitingReceipt,
    Known(String),
}

struct Slot {
    generation: u64,
    dispatch: Arc<RequestDispatch>,
    state: Mutex<SlotState>,
}

struct SlotState {
    closed: bool,
    task: TaskOwner,
    seen: Vec<Value>,
    pending: VecDeque<ServerRequest>,
    active: Option<(Value, Arc<AtomicBool>, Arc<AtomicBool>)>,
    waiter: Option<Waker>,
}

struct ServerRequest {
    id: Value,
    params: Value,
    related_task: Option<String>,
    cancelled: Arc<AtomicBool>,
    responding: Arc<AtomicBool>,
    authorized: bool,
}

/// Connection-owned form request routing. Custom transports may opt in by
/// exposing this registry and implementing generation-pinned server replies.
#[derive(Default)]
pub struct ElicitationRegistry {
    next_id: AtomicU64,
    slots: Mutex<HashMap<u64, Arc<Slot>>>,
}

impl std::fmt::Debug for ElicitationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ElicitationRegistry")
            .finish_non_exhaustive()
    }
}

impl ElicitationRegistry {
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        dispatch: Arc<RequestDispatch>,
        generation: u64,
        task_augmented: bool,
    ) -> Option<Subscription> {
        let id = self
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        let slot = Arc::new(Slot {
            generation,
            dispatch,
            state: Mutex::new(SlotState {
                closed: false,
                task: if task_augmented {
                    TaskOwner::AwaitingReceipt
                } else {
                    TaskOwner::Synchronous
                },
                seen: Vec::new(),
                pending: VecDeque::new(),
                active: None,
                waiter: None,
            }),
        });
        let mut slots = lock(&self.slots);
        if slots.len() >= MAX_CALLS {
            return None;
        }
        slots.insert(id, Arc::clone(&slot));
        Some(Subscription {
            registry: Arc::clone(self),
            id,
            slot,
        })
    }

    /// Return an immediate protocol response, or enqueue the request for its
    /// caller. This never compiles schemas, invokes a UI, or performs I/O.
    #[must_use]
    pub fn route(&self, id: &Value, params: Option<&Value>, generation: u64) -> Option<Value> {
        if !matches!(id, Value::Number(_) | Value::String(_))
            || id.as_str().is_some_and(|id| id.len() > 1024)
        {
            return Some(rpc_error(&Value::Null, -32600, "invalid elicitation request id"));
        }
        let slots = lock(&self.slots);
        for slot in slots.values() {
            if slot.generation == generation && slot.dispatch.is_claimed() {
                let state = lock(&slot.state);
                if state.seen.iter().any(|seen| seen == id) {
                    // Even a malformed replay must not race the original
                    // answer with a second JSON-RPC response for its ID.
                    return None;
                }
            }
        }
        let Some(params) = params.filter(|params| params.is_object()) else {
            return Some(rpc_error(id, -32602, "elicitation params must be an object"));
        };
        if !bounded(params, MAX_FORM_BYTES)
            || params.get("task").is_some()
            || params.get("mode").is_some_and(|mode| mode.as_str() != Some("form"))
        {
            return Some(rpc_error(id, -32602, "only bounded, non-task-augmented form elicitation is supported"));
        }
        let related_task = match params.get("_meta") {
            None => None,
            Some(Value::Object(meta)) => match meta.get(RELATED_TASK) {
                None => None,
                Some(related) => {
                    let Some(task) = related.get("taskId").and_then(Value::as_str)
                        .filter(|task| !task.is_empty() && task.len() <= 4096)
                    else {
                        return Some(rpc_error(id, -32602, "invalid related-task metadata"));
                    };
                    Some(task)
                }
            },
            Some(_) => return Some(rpc_error(id, -32602, "elicitation metadata must be an object")),
        };
        let mut exact = Vec::new();
        let mut awaiting = Vec::new();
        for slot in slots.values() {
            if slot.generation != generation || !slot.dispatch.is_claimed() {
                continue;
            }
            let state = lock(&slot.state);
            if state.closed {
                continue;
            }
            match (related_task, &state.task) {
                (None, _) => exact.push(Arc::clone(slot)),
                (Some(task), TaskOwner::Known(owned)) if task == owned => {
                    exact.push(Arc::clone(slot));
                }
                (Some(_), TaskOwner::AwaitingReceipt) => awaiting.push(Arc::clone(slot)),
                _ => {}
            }
        }
        // The reader can outrun the caller after a create-task receipt. Queue
        // tentatively for one unambiguous awaiting call, but do not show UI
        // until that caller has validated and bound the actual task identity.
        let candidates = if exact.is_empty() && related_task.is_some() {
            &awaiting
        } else {
            &exact
        };
        let [slot] = candidates.as_slice() else {
            return Some(action(id, "cancel", related_task));
        };
        let mut state = lock(&slot.state);
        if state.closed || state.seen.len() >= MAX_REQUESTS_PER_CALL
            || state.pending.len() >= 2
            || state.pending.iter().any(|request| !request.cancelled.load(Ordering::Acquire))
            || state.active.as_ref().is_some_and(|(_, cancelled, responding)| {
                !cancelled.load(Ordering::Acquire) && !responding.load(Ordering::Acquire)
            })
        {
            return Some(action(id, "cancel", related_task));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let responding = Arc::new(AtomicBool::new(false));
        state.seen.push(id.clone());
        state.pending.push_back(ServerRequest {
            id: id.clone(),
            params: params.clone(),
            related_task: related_task.map(str::to_string),
            cancelled,
            responding,
            authorized: false,
        });
        let waiter = state.waiter.take();
        drop(state);
        drop(slots);
        if let Some(waiter) = waiter {
            waiter.wake();
        }
        None
    }

    /// Server cancellation dismisses the exact queued or visible form. It
    /// cannot cancel the caller's tool or a newer connection's request.
    pub fn cancel(&self, params: Option<&Value>, generation: u64) {
        let Some(id) = params.and_then(|params| params.get("requestId")) else {
            return;
        };
        let mut waiters = Vec::new();
        for slot in lock(&self.slots).values() {
            if slot.generation != generation {
                continue;
            }
            let mut state = lock(&slot.state);
            let mut matched = false;
            if let Some((active, cancelled, _)) = &state.active
                && active == id
            {
                cancelled.store(true, Ordering::Release);
                matched = true;
            }
            for request in &state.pending {
                if request.id == *id {
                    request.cancelled.store(true, Ordering::Release);
                    matched = true;
                }
            }
            if matched && let Some(waiter) = state.waiter.take() {
                waiters.push(waiter);
            }
        }
        for waiter in waiters {
            waiter.wake();
        }
    }
}

pub(crate) struct Subscription {
    registry: Arc<ElicitationRegistry>,
    id: u64,
    slot: Arc<Slot>,
}

#[derive(Clone)]
pub(crate) struct TaskBinding(Arc<Slot>);

impl TaskBinding {
    pub(crate) fn bind(&self, task_id: &str) {
        let waiter = {
            let mut state = lock(&self.0.state);
            if state.closed || !matches!(state.task, TaskOwner::AwaitingReceipt) {
                return;
            }
            state.task = TaskOwner::Known(task_id.to_string());
            state.waiter.take()
        };
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

impl Subscription {
    pub(crate) fn task_binding(&self) -> TaskBinding {
        TaskBinding(Arc::clone(&self.slot))
    }

    fn poll_request(&self, task: &mut Context<'_>, finished: bool) -> Poll<ServerRequest> {
        let mut state = lock(&self.slot.state);
        state.waiter = Some(task.waker().clone());
        let ready = state.pending.front().is_some_and(|request| {
            finished || request.cancelled.load(Ordering::Acquire)
                || request.related_task.is_none()
                || !matches!(state.task, TaskOwner::AwaitingReceipt)
        });
        if !ready {
            return Poll::Pending;
        }
        let mut request = state.pending.pop_front().expect("pending request checked");
        request.authorized = !finished && match (&request.related_task, &state.task) {
            (None, _) => true,
            (Some(task), TaskOwner::Known(owned)) => task == owned,
            _ => false,
        };
        state.active = Some((request.id.clone(), Arc::clone(&request.cancelled), Arc::clone(&request.responding)));
        Poll::Ready(request)
    }

    fn complete(&self) {
        lock(&self.slot.state).active = None;
    }

    fn close_admission(&self) {
        lock(&self.slot.state).closed = true;
    }

    fn has_pending(&self) -> bool {
        !lock(&self.slot.state).pending.is_empty()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut slots = lock(&self.registry.slots);
        let mut state = lock(&self.slot.state);
        state.closed = true;
        if let Some((_, cancelled, _)) = &state.active {
            cancelled.store(true, Ordering::Release);
        }
        state.pending.clear();
        state.waiter.take();
        slots.remove(&self.id);
    }
}

/// Escape controls, bidi text, markup, and URL punctuation before displaying
/// server strings. The full escaped value is shown; data is never truncated
/// into a misleading preview or rendered as a clickable server-supplied URL.
fn display(value: &str) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for character in value.chars() {
        if matches!(character, '<' | '>' | '[' | ']' | '(' | ')' | '`' | '*' | '_' | '~' | '#' | '|' | '!' | ':' | '@') {
            let _ = write!(text, "\\u{{{:x}}}", u32::from(character));
        } else {
            text.extend(character.escape_default());
        }
    }
    text
}

fn only_keys(object: &Map<String, Value>, keys: &[&str]) -> bool {
    object.keys().all(|key| keys.contains(&key.as_str()))
}

fn optional_text(object: &Map<String, Value>, key: &str, max: usize) -> Option<()> {
    match object.get(key) {
        None => Some(()),
        Some(Value::String(value)) if value.len() <= max => Some(()),
        _ => None,
    }
}

struct Field {
    name: String,
    schema: McpOutputSchema,
    choices: Vec<(String, Value)>,
    required: bool,
    string: bool,
}

fn choices(schema: &Map<String, Value>, alternatives: &str) -> Option<Vec<(String, Value)>> {
    if let Some(values) = schema.get("enum") {
        let values = values.as_array()?;
        if values.is_empty() || values.len() > MAX_CHOICES {
            return None;
        }
        let names = match schema.get("enumNames") {
            None => None,
            Some(names) => Some(names.as_array()?),
        };
        if names.is_some_and(|names| names.len() != values.len()) {
            return None;
        }
        return values.iter().enumerate().map(|(index, value)| {
            let value = value.as_str().filter(|value| value.len() <= MAX_VALUE_BYTES)?;
            let title = names.map_or(Some(value), |names| names[index].as_str())?;
            (title.len() <= 2048).then(|| (title.to_string(), Value::String(value.to_string())))
        }).collect();
    }
    let values = schema.get(alternatives)?.as_array()?;
    if values.is_empty() || values.len() > MAX_CHOICES {
        return None;
    }
    values.iter().map(|value| {
        let object = value.as_object()?;
        if !only_keys(object, &["const", "title"]) {
            return None;
        }
        let value = object.get("const")?.as_str().filter(|value| value.len() <= MAX_VALUE_BYTES)?;
        let title = object.get("title")?.as_str().filter(|title| title.len() <= 2048)?;
        Some((title.to_string(), Value::String(value.to_string())))
    }).collect()
}

impl Field {
    fn parse(name: &str, schema: &Value, required: bool) -> Option<Self> {
        let object = schema.as_object()?;
        optional_text(object, "title", 2048)?;
        optional_text(object, "description", 2048)?;
        let kind = object.get("type")?.as_str()?;
        let choices = match kind {
            "string" => {
                if !only_keys(object, &["type", "title", "description", "default", "minLength", "maxLength", "pattern", "format", "enum", "enumNames", "oneOf"])
                    || (object.contains_key("enum") && object.contains_key("oneOf"))
                    || (object.contains_key("enumNames") && !object.contains_key("enum"))
                {
                    return None;
                }
                if let Some(format) = object.get("format")
                    && !matches!(format.as_str(), Some("email" | "uri" | "date" | "date-time"))
                {
                    return None;
                }
                if object.contains_key("enum") || object.contains_key("oneOf") {
                    choices(object, "oneOf")?
                } else {
                    Vec::new()
                }
            }
            "number" | "integer" => {
                if !only_keys(object, &["type", "title", "description", "default", "minimum", "maximum"]) {
                    return None;
                }
                Vec::new()
            }
            "boolean" => {
                if !only_keys(object, &["type", "title", "description", "default"]) {
                    return None;
                }
                vec![("true".to_string(), json!(true)), ("false".to_string(), json!(false))]
            }
            "array" => {
                if !only_keys(object, &["type", "title", "description", "default", "minItems", "maxItems", "items"]) {
                    return None;
                }
                let items = object.get("items")?.as_object()?;
                if !only_keys(items, &["type", "enum", "anyOf"])
                    || (items.contains_key("enum") && items.contains_key("anyOf"))
                    || (items.contains_key("enum") && items.get("type").and_then(Value::as_str) != Some("string"))
                {
                    return None;
                }
                choices(items, "anyOf")?;
                Vec::new()
            }
            _ => return None,
        };
        let compiled = McpOutputSchema::compile_form(schema).ok()?;
        if let Some(default) = schema.get("default")
            && (!bounded(default, MAX_VALUE_BYTES) || !compiled.accepts(default))
        {
            return None;
        }
        Some(Self {
            name: name.to_string(),
            schema: compiled,
            choices,
            required,
            string: kind == "string",
        })
    }

    fn input_value(&self, input: &str) -> Option<Value> {
        if input.len() > MAX_VALUE_BYTES {
            return None;
        }
        let value = if self.string {
            // Quotes let users enter an empty string or literal newlines;
            // ordinary free text remains ordinary string content.
            if input.starts_with('"') {
                serde_json::from_str::<String>(input).ok().map(Value::String)?
            } else {
                Value::String(input.to_string())
            }
        } else {
            serde_json::from_str(input).ok()?
        };
        (bounded(&value, MAX_VALUE_BYTES) && self.schema.accepts(&value)).then_some(value)
    }
}

struct Form {
    message: String,
    fields: Vec<Field>,
    schema: McpOutputSchema,
}

impl Form {
    fn parse(params: &Value) -> Option<Self> {
        if !bounded(params, MAX_FORM_BYTES) {
            return None;
        }
        let message = params.get("message")?.as_str().filter(|message| message.len() <= 4096)?;
        let schema = params.get("requestedSchema")?;
        let object = schema.as_object()?;
        if object.get("type").and_then(Value::as_str) != Some("object")
            || !only_keys(object, &["$schema", "type", "properties", "required"])
        {
            return None;
        }
        let properties = object.get("properties")?.as_object()?;
        if properties.len() > MAX_FIELDS {
            return None;
        }
        let mut required = HashSet::new();
        if let Some(values) = object.get("required") {
            for value in values.as_array()? {
                let name = value.as_str()?;
                if !properties.contains_key(name) || !required.insert(name) {
                    return None;
                }
            }
        }
        let fields = properties.iter().map(|(name, schema)| {
            if name.is_empty() || name.len() > 256 {
                return None;
            }
            Field::parse(name, schema, required.contains(name.as_str()))
        }).collect::<Option<Vec<_>>>()?;
        Some(Self {
            message: message.to_string(),
            fields,
            schema: McpOutputSchema::compile_form(schema).ok()?,
        })
    }
}

enum Answer {
    Choice(String),
    Text(String),
    Cancel,
}

async fn prompt(ask: &AskTool, id: &str, question: String, options: Vec<String>) -> Answer {
    let request = AskRequest {
        questions: vec![AskQuestion {
            id: Some(id.to_string()),
            question,
            header: Some("MCP form".to_string()),
            options: options.into_iter().map(|label| AskOption { label, description: None }).collect(),
            recommended: None,
            multi: false,
        }],
    };
    let Ok(response) = ask.prompt_installed(request).await else {
        return Answer::Cancel;
    };
    if response.dismissed || response.answers.len() != 1 {
        return Answer::Cancel;
    }
    let answer = &response.answers[0];
    if answer.question_id != id {
        return Answer::Cancel;
    }
    match (answer.selected.as_slice(), answer.other.as_ref()) {
        ([choice], None) => Answer::Choice(choice.clone()),
        ([], Some(text)) if text.len() <= MAX_VALUE_BYTES => Answer::Text(text.clone()),
        _ => Answer::Cancel,
    }
}

async fn fill_form(ask: &AskTool, server: &str, form: &Form, request: &ServerRequest) -> Value {
    let origin = format!("MCP server \"{}\" requests non-sensitive information.\nReason: \"{}\"", display(server), display(&form.message));
    let mut content = Map::new();
    for _ in 0..4 {
        for field in &form.fields {
            let schema = field.schema.schema();
            let default = content.get(&field.name).or_else(|| schema.get("default"));
            let mut options = Vec::new();
            let mut selected_values = Vec::new();
            let controls = 2 + usize::from(!field.required) + usize::from(default.is_some());
            if field.choices.len() <= crate::ask::MAX_OPTIONS.saturating_sub(controls) {
                for (index, (title, value)) in field.choices.iter().enumerate() {
                    let label = format!("Value {}: {}", index + 1, display(title));
                    options.push(label.clone());
                    selected_values.push((label, value.clone()));
                }
            }
            if default.is_some() {
                options.push(KEEP.to_string());
            }
            if !field.required {
                options.push(OMIT.to_string());
            }
            options.extend([DECLINE.to_string(), CANCEL.to_string()]);
            let previous = default.map(|value| format!("\nDisplayed value: {}", display(&value.to_string()))).unwrap_or_default();
            let title = schema.get("title").and_then(Value::as_str).unwrap_or(&field.name);
            let description = schema.get("description").and_then(Value::as_str)
                .map(|value| format!("\n{}", display(value))).unwrap_or_default();
            let guidance = if field.string {
                "Use Other to enter text (JSON quotes can express an empty string or escapes)."
            } else {
                "Use Other to enter a JSON value: a number, true/false, or an array of enum strings."
            };
            let question = format!("{origin}\n\nField: \"{}\" ({}){description}\n{guidance}{previous}\nConstraints: {}\nValues are sent only after you review and submit the complete form.", display(title), if field.required { "required" } else { "optional" }, display(&schema.to_string()));
            let default = default.cloned();
            let mut answered = false;
            for attempt in 0..3 {
                let question = if attempt == 0 { question.clone() } else {
                    format!("{question}\nThe previous value did not match this field's constraints. Please correct it or decline/cancel.")
                };
                let value = match prompt(ask, "mcp_field", question, options.clone()).await {
                    Answer::Cancel => return action(&request.id, "cancel", request.related_task.as_deref()),
                    Answer::Choice(choice) if choice == CANCEL => return action(&request.id, "cancel", request.related_task.as_deref()),
                    Answer::Choice(choice) if choice == DECLINE => return action(&request.id, "decline", request.related_task.as_deref()),
                    Answer::Choice(choice) if choice == OMIT && !field.required => {
                        content.remove(&field.name);
                        answered = true;
                        break;
                    }
                    Answer::Choice(choice) if choice == KEEP => default.clone(),
                    Answer::Choice(choice) => selected_values.iter().find(|(label, _)| *label == choice).map(|(_, value)| value.clone()),
                    Answer::Text(text) => field.input_value(&text),
                };
                if let Some(value) = value
                    && bounded(&value, MAX_VALUE_BYTES)
                    && field.schema.accepts(&value)
                {
                    content.insert(field.name.clone(), value);
                    answered = true;
                    break;
                }
            }
            if !answered {
                return action(&request.id, "cancel", request.related_task.as_deref());
            }
        }
        let value = Value::Object(content.clone());
        if !bounded(&value, MAX_CONTENT_BYTES) || !form.schema.accepts(&value) {
            return action(&request.id, "cancel", request.related_task.as_deref());
        }
        let review = format!("{origin}\n\nReview the complete values below. Controls, markup, and URL punctuation are escaped for display. Submit sends these values to this server. Edit lets you change them first.\n\n{}", display(&value.to_string()));
        match prompt(ask, "mcp_review", review, vec![SUBMIT.to_string(), EDIT.to_string(), DECLINE.to_string(), CANCEL.to_string()]).await {
            Answer::Choice(choice) if choice == SUBMIT => {
                let mut response = action(&request.id, "accept", request.related_task.as_deref());
                response["result"]["content"] = value;
                return response;
            }
            Answer::Choice(choice) if choice == EDIT => {}
            Answer::Choice(choice) if choice == DECLINE => return action(&request.id, "decline", request.related_task.as_deref()),
            _ => return action(&request.id, "cancel", request.related_task.as_deref()),
        }
    }
    action(&request.id, "cancel", request.related_task.as_deref())
}

struct Active<'a> {
    id: Value,
    related_task: Option<String>,
    cancelled: Arc<AtomicBool>,
    responding: Arc<AtomicBool>,
    cancelling: bool,
    future: BoxFuture<'a, Result<()>>,
}

/// Poll protocol work and its user interaction on the same caller. Completing
/// the tool closes admission and cancels any accepted pending form before
/// returning, so a reader cannot leave an unanswered request or orphaned card.
pub(crate) async fn observe<'a>(
    operation: impl Future<Output = Result<Value>> + Send + 'a,
    subscription: Option<Subscription>,
    ask: Option<AskTool>,
    server: &'a str,
    transport: &'a Arc<dyn McpTransport>,
    current: impl Fn() -> bool + Send + Sync + 'a,
) -> Result<Value> {
    let Some(subscription) = subscription else {
        return operation.await;
    };
    let generation = subscription.slot.generation;
    let current = Arc::new(current);
    let mut operation = std::pin::pin!(operation);
    let mut completed = None;
    let mut active: Option<Active<'a>> = None;
    poll_fn(|task| {
        if !current() {
            return Poll::Ready(Err(failure("elicitation caller is no longer active")));
        }
        if completed.is_none()
            && let Poll::Ready(result) = operation.as_mut().poll(task)
        {
            subscription.close_admission();
            completed = Some(result);
        }
        if !current() {
            return Poll::Ready(Err(failure("elicitation caller changed during protocol work")));
        }
        if let Some(pending) = active.as_mut()
            && !pending.cancelling
            && !pending.responding.load(Ordering::Acquire)
            && (completed.is_some() || pending.cancelled.load(Ordering::Acquire))
        {
            // Replacing this future drops AskTool's pending-reply lease.
            let response = action(&pending.id, "cancel", pending.related_task.as_deref());
            pending.future = Box::pin(async move {
                send_response(transport, response, generation).await
            });
            pending.cancelling = true;
        }
        if active.is_none()
            && let Poll::Ready(request) = subscription.poll_request(task, completed.is_some())
        {
            let id = request.id.clone();
            let related_task = request.related_task.clone();
            let cancelled = Arc::clone(&request.cancelled);
            let responding = Arc::clone(&request.responding);
            let response_started = Arc::clone(&responding);
            let current = Arc::clone(&current);
            let ask = ask.clone();
            let cancelling = !request.authorized || cancelled.load(Ordering::Acquire);
            active = Some(Active {
                id,
                related_task,
                cancelled,
                responding,
                cancelling,
                future: Box::pin(async move {
                    let mut response = if cancelling {
                        action(&request.id, "cancel", request.related_task.as_deref())
                    } else if let Some(form) = Form::parse(&request.params) {
                        if let Some(ask) = ask {
                            fill_form(&ask, server, &form, &request).await
                        } else {
                            action(&request.id, "cancel", request.related_task.as_deref())
                        }
                    } else {
                        rpc_error(&request.id, -32602, "invalid, oversized, or unsupported form schema")
                    };
                    if !current() {
                        return Err(failure("elicitation caller changed before its response"));
                    }
                    if request.cancelled.load(Ordering::Acquire) {
                        response = action(&request.id, "cancel", request.related_task.as_deref());
                    }
                    response_started.store(true, Ordering::Release);
                    send_response(transport, response, generation).await
                }),
            });
        }
        if let Some(pending) = active.as_mut() {
            match pending.future.as_mut().poll(task) {
                Poll::Ready(Ok(())) => {
                    active = None;
                    subscription.complete();
                    task.waker().wake_by_ref();
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {}
            }
        }
        if active.is_none() && !subscription.has_pending()
            && let Some(result) = completed.take()
        {
            return Poll::Ready(result);
        }
        Poll::Pending
    }).await
}

async fn send_response(
    transport: &Arc<dyn McpTransport>,
    response: Value,
    generation: u64,
) -> Result<()> {
    transport.respond_to_server_request(response, generation).await.map_err(|_| {
        Error::tool("mcp", "[MCP_DELIVERY_INDETERMINATE] elicitation response could not be delivered in the originating protocol session; remote effects may already have occurred and the tool was not replayed")
    })
}
