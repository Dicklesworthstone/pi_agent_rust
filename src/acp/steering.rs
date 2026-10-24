//! Turn-owned ACP input. Dispatch never waits for a running prompt: requests
//! retain their response IDs here until a native queue boundary claims them.
//! Idle or unclaimed terminal input stays with the client (`promptRequired`).

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::agent::{MessageFetcher, QueuedAgentMessage};
use crate::model::{ContentBlock, Message, UserContent, UserMessage};

use super::{AcpOutput, content, json_rpc_ok};

const MAX_PENDING_INPUTS: usize = 100;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_AUTHORED_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Queue {
    Steering,
    FollowUp,
}

impl Queue {
    const fn name(self) -> &'static str {
        match self {
            Self::Steering => "steering",
            Self::FollowUp => "followUp",
        }
    }
}

pub(super) struct Input {
    response_id: Option<Value>,
    queue: Queue,
    message: QueuedAgentMessage,
    bytes: usize,
}

impl Input {
    pub(super) fn parse(params: &Value, response_id: Option<Value>) -> Result<Self, String> {
        if response_id
            .as_ref()
            .is_some_and(|id| !matches!(id, Value::Null | Value::String(_) | Value::Number(_)))
        {
            return Err("JSON-RPC request id must be a string, number, or null".into());
        }
        let owned;
        let blocks = match params.get("prompt") {
            Some(Value::Array(blocks)) => blocks,
            Some(Value::String(text)) => {
                owned = vec![json!({ "type": "text", "text": text })];
                &owned
            }
            _ => return Err("prompt must be an array of content blocks or a string".into()),
        };
        if blocks.is_empty() || blocks.len() > 128 {
            return Err("steering requires between 1 and 128 content blocks".into());
        }
        // These fields are optional metadata, but a malformed field must not
        // disappear merely because the converter does not forward it.
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("resource_link") {
                for key in ["title", "name", "mimeType", "description"] {
                    if block
                        .get(key)
                        .is_some_and(|value| !value.is_null() && !value.is_string())
                    {
                        return Err(format!("resource_link {key} must be a string or null"));
                    }
                }
                if block
                    .get("size")
                    .is_some_and(|value| !value.is_null() && !value.is_number())
                {
                    return Err("resource_link size must be a number or null".into());
                }
            }
            if block.get("type").and_then(Value::as_str) == Some("resource")
                && block
                    .get("resource")
                    .and_then(|resource| resource.get("mimeType"))
                    .is_some_and(|mime| !mime.is_string())
            {
                return Err("embedded resource mimeType must be a string when present".into());
            }
        }
        let native = content::extract_prompt_content(blocks)?;
        if !native.iter().any(|block| match block {
            ContentBlock::Text(text) => !text.text.trim().is_empty(),
            ContentBlock::Image(image) => !image.data.is_empty(),
            _ => false,
        }) {
            return Err("steering prompt is empty".into());
        }
        // Only top-level user prose may trigger magic keywords. Embedded
        // documents and URI labels retain their reference provenance.
        let authored = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if authored.len() > MAX_AUTHORED_BYTES {
            return Err("steering authored text exceeds 256 KiB".into());
        }
        let native = UserContent::Blocks(native);
        let mut budget = ByteBudget(MAX_INPUT_BYTES);
        serde_json::to_writer(&mut budget, &native)
            .map_err(|_| "steering content exceeds 4 MiB when serialized".to_string())?;
        serde_json::to_writer(&mut budget, &response_id)
            .map_err(|_| "steering content and request id exceed 4 MiB".to_string())?;
        let bytes = MAX_INPUT_BYTES - budget.0 + authored.len();
        let queue = if params
            .pointer("/_meta/steering/queue")
            .and_then(Value::as_str)
            == Some("followUp")
        {
            Queue::FollowUp
        } else {
            Queue::Steering
        };
        Ok(Self {
            response_id,
            queue,
            message: QueuedAgentMessage::authored(
                Message::User(UserMessage {
                    content: native,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                }),
                authored,
            ),
            bytes,
        })
    }

    pub(super) fn is_steering(&self) -> bool {
        self.queue == Queue::Steering
    }

    pub(super) fn reply(&self, output: &AcpOutput, outcome: &str) -> bool {
        self.response_id.as_ref().is_none_or(|id| {
            output
                .send(json_rpc_ok(
                    id.clone(),
                    json!({ "outcome": outcome, "queue": self.queue.name() }),
                ))
                .is_ok()
        })
    }

    pub(super) fn reply_full(&self, output: &AcpOutput) {
        if let Some(id) = &self.response_id {
            let _ = output.send(json_rpc_ok(
                id.clone(),
                json!({
                    "outcome": "failed",
                    "reason": "ACP steering queue is full; no input was accepted",
                }),
            ));
        }
    }
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("ACP steering content limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Mailbox {
    state: Mutex<State>,
}

struct State {
    accepting: bool,
    retired: bool,
    pending: VecDeque<Input>,
    bytes: usize,
}

impl Mailbox {
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                accepting: true,
                retired: false,
                pending: VecDeque::new(),
                bytes: 0,
            }),
        }
    }

    /// Err retains the caller's input; no partial admission or truncation.
    pub(super) fn enqueue(&self, input: Input) -> Result<(), (Box<Input>, bool)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retired {
            return Err((Box::new(input), false));
        }
        if state.pending.len() >= MAX_PENDING_INPUTS
            || input.bytes > MAX_PENDING_BYTES.saturating_sub(state.bytes)
        {
            return Err((Box::new(input), true));
        }
        state.bytes += input.bytes;
        state.pending.push_back(input);
        Ok(())
    }

    pub(super) fn permits_tool(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting && !state.pending.iter().any(Input::is_steering)
    }

    /// Stop native delivery but retain response IDs through owner cleanup.
    /// A client may resubmit promptRequired as soon as it sees the response;
    /// therefore only retirement, after the prompt reservation is removed,
    /// may publish that outcome.
    pub(super) fn seal(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting = false;
    }

    pub(super) fn close(&self) -> Vec<Input> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        state.retired = true;
        state.bytes = 0;
        std::mem::take(&mut state.pending).into_iter().collect()
    }

    fn fetch(&self, queue: Queue) -> Option<Input> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return None;
        }
        let position = state
            .pending
            .iter()
            .position(|input| input.queue == queue)?;
        let input = state.pending.remove(position)?;
        state.bytes -= input.bytes;
        Some(input)
    }

    pub(super) fn fetcher(self: &Arc<Self>, output: &AcpOutput, queue: Queue) -> MessageFetcher {
        let mailbox = Arc::clone(self);
        let output = output.clone();
        Arc::new(move || {
            let mailbox = Arc::clone(&mailbox);
            let output = output.clone();
            Box::pin(async move {
                if crate::agent_cx::AgentCx::for_current_or_request()
                    .checkpoint()
                    .is_err()
                {
                    return Vec::new();
                }
                let Some(input) = mailbox.fetch(queue) else {
                    return Vec::new();
                };
                if input.reply(&output, "injected") {
                    vec![input.message]
                } else {
                    Vec::new()
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: Value, queue: &str) -> Input {
        Input::parse(
            &json!({
                "prompt": prompt,
                "_meta": { "steering": { "queue": queue } },
            }),
            Some(json!(1)),
        )
        .expect("valid queued prompt")
    }

    #[test]
    fn native_images_and_reference_content_preserve_authored_provenance() {
        let input = input(
            json!([
                { "type": "text", "text": "Change the selected function" },
                { "type": "resource", "resource": {
                    "uri": "file:///unopened/private.rs",
                    "text": "ultrathink is only an identifier in the attached file",
                } },
                { "type": "image", "mimeType": "image/png", "data": "AQID" },
            ]),
            "steering",
        );
        assert_eq!(
            input.message.keyword_scan_source(),
            Some("Change the selected function")
        );
        let Message::User(user) = input.message.message() else {
            panic!("user input")
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("native blocks")
        };
        assert!(
            matches!(&blocks[1], ContentBlock::Text(text) if text.text.contains("file:///unopened/private.rs"))
        );
        assert!(matches!(&blocks[2], ContentBlock::Image(image) if image.data == "AQID"));
    }

    #[test]
    fn malformed_or_empty_prompts_are_rejected_before_admission() {
        for prompt in [
            json!([]),
            json!("   "),
            json!([null]),
            json!([{}]),
            json!([{ "type": "text", "text": 7 }]),
            json!([{ "type": "image", "mimeType": "image/png", "data": "?" }]),
            json!([{ "type": "resource_link", "uri": "file:///x", "size": "large" }]),
            json!([{ "type": "resource_link", "uri": "file:///x", "description": 7 }]),
            json!([{ "type": "resource", "resource": { "uri": "file:///x", "text": "x", "mimeType": null } }]),
        ] {
            assert!(Input::parse(&json!({ "prompt": prompt }), Some(json!(1))).is_err());
        }
        assert!(Input::parse(&json!({ "prompt": "ok" }), Some(json!({ "bad": "id" }))).is_err());
        assert!(
            Input::parse(
                &json!({ "prompt": "ok" }),
                Some(json!("x".repeat(MAX_INPUT_BYTES))),
            )
            .is_err()
        );
    }

    #[test]
    fn queues_are_bounded_and_never_truncate_or_accept_after_close() {
        let mailbox = Mailbox::new();
        for _ in 0..MAX_PENDING_INPUTS {
            assert!(mailbox.enqueue(input(json!("queued"), "followUp")).is_ok());
        }
        assert!(matches!(
            mailbox.enqueue(input(json!("overflow"), "steering")),
            Err((_, true))
        ));
        assert!(
            mailbox.permits_tool(),
            "follow-ups do not supersede permission requests"
        );
        assert_eq!(mailbox.close().len(), MAX_PENDING_INPUTS);
        assert!(matches!(
            mailbox.enqueue(input(json!("late"), "steering")),
            Err((_, false))
        ));
        assert!(!mailbox.permits_tool());

        assert!(
            Input::parse(
                &json!({ "prompt": "x".repeat(MAX_AUTHORED_BYTES + 1) }),
                None,
            )
            .is_err()
        );
        let mailbox = Mailbox::new();
        let large = json!([{ "type": "resource", "resource": {
            "uri": "file:///x", "text": "x".repeat(3 * 1024 * 1024),
        } }]);
        assert!(mailbox.enqueue(input(large.clone(), "followUp")).is_ok());
        assert!(mailbox.enqueue(input(large.clone(), "followUp")).is_ok());
        assert!(matches!(
            mailbox.enqueue(input(large, "followUp")),
            Err((_, true))
        ));
        assert_eq!(mailbox.close().len(), 2);
    }

    #[test]
    fn native_fetchers_preserve_lane_order_and_do_not_consume_before_poll() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tx, rx) = std::sync::mpsc::sync_channel(8);
            let output = AcpOutput::new(tx);
            let mailbox = Arc::new(Mailbox::new());
            assert!(mailbox.enqueue(input(json!("later"), "followUp")).is_ok());
            assert!(mailbox.enqueue(input(json!("first"), "steering")).is_ok());
            let fetch = mailbox.fetcher(&output, Queue::Steering);
            let unpolled = fetch();
            drop(unpolled);
            assert!(rx.try_recv().is_err());
            assert!(!mailbox.permits_tool());
            let delivered = fetch().await;
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].keyword_scan_source(), Some("first"));
            assert!(mailbox.permits_tool());
            let frame: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(
                frame["result"],
                json!({ "outcome": "injected", "queue": "steering" })
            );
            let pending = mailbox.close();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].message.keyword_scan_source(), Some("later"));
            assert!(fetch().await.is_empty());
        });
    }
}
